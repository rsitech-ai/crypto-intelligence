use std::collections::BTreeMap;

use dataset::{
    CorrectionPolicy, DatasetError, DatasetManifest, DatasetManifestInput, EventLabel,
    FeatureValue, FoldBuilder, JoinInput, Sample, UniverseMembership, WalkForwardSchedule,
    join_point_in_time,
};
use labels::{ExclusionReason, LabelOutcome};
use semver::Version;

const DAY_SECONDS: u64 = 86_400;
const DAY_NS: i64 = 86_400_000_000_000;

#[test]
fn join_rejects_feature_known_after_prediction_and_preserves_missingness() {
    let mut input = join_input();
    input.features[0].as_known_at_ns = input.origin_time_ns + 1;
    assert_eq!(
        join_point_in_time(input),
        Err(DatasetError::FeatureKnownAfterOrigin {
            feature_id: "return_1h".to_owned(),
        })
    );

    let mut input = join_input();
    input.features[0].value = None;
    let row = join_point_in_time(input).expect("explicit missingness is valid");
    assert_eq!(row.features().get("return_1h"), Some(&None));
}

#[test]
fn universe_instrument_revision_and_label_cutoff_are_point_in_time() {
    let mut future_universe = join_input();
    future_universe.universe.as_known_at_ns = future_universe.origin_time_ns + 1;
    assert_eq!(
        join_point_in_time(future_universe),
        Err(DatasetError::UniverseKnownAfterOrigin)
    );

    let mut delisted = join_input();
    delisted.universe.valid_to_ns = Some(delisted.origin_time_ns);
    assert_eq!(
        join_point_in_time(delisted),
        Err(DatasetError::OutsideUniverse)
    );

    let mut corrected_late = join_input();
    corrected_late.features[0].revision = 2;
    corrected_late.features[0].as_known_at_ns = corrected_late.origin_time_ns + 1;
    assert!(matches!(
        join_point_in_time(corrected_late),
        Err(DatasetError::FeatureKnownAfterOrigin { .. })
    ));

    let mut late_label = join_input();
    late_label.labels[0].outcome_known_at_ns = late_label.dataset_cutoff_ns + 1;
    assert_eq!(
        join_point_in_time(late_label),
        Err(DatasetError::LabelKnownAfterCutoff {
            label_id: "downside_5pct".to_owned(),
        })
    );
}

#[test]
fn labels_must_share_origin_and_censored_outcomes_remain_explicit() {
    let mut wrong_origin = join_input();
    wrong_origin.labels[0].origin_time_ns += 1;
    assert_eq!(
        join_point_in_time(wrong_origin),
        Err(DatasetError::LabelOriginMismatch {
            label_id: "downside_5pct".to_owned(),
        })
    );

    let mut censored = join_input();
    censored.labels[0].outcome = LabelOutcome::Excluded(ExclusionReason::UnresolvedCorrection);
    let row = join_point_in_time(censored).expect("explicit exclusion is retained");
    assert_eq!(
        row.labels()[0].outcome,
        LabelOutcome::Excluded(ExclusionReason::UnresolvedCorrection)
    );
}

#[test]
fn manifest_is_canonical_bounded_and_all_semantics_change_identity() {
    let base = manifest_input();
    let manifest = DatasetManifest::try_new(base.clone()).expect("valid manifest");
    let same = DatasetManifest::try_new(base.clone()).expect("repeat manifest");
    assert_eq!(manifest.manifest_hash(), same.manifest_hash());
    assert_eq!(
        manifest.manifest_hash(),
        [
            248, 165, 61, 242, 241, 168, 45, 138, 153, 247, 29, 9, 110, 141, 76, 239, 32, 109, 202,
            74, 76, 29, 135, 17, 50, 110, 212, 201, 201, 182, 2, 189,
        ]
    );

    let mut changed = base.clone();
    changed.as_known_at_cutoff_ns += 1;
    assert_manifest_hash_change(manifest.manifest_hash(), changed);
    let mut changed = base.clone();
    changed.dataset_id = "eth_transition_training".to_owned();
    assert_manifest_hash_change(manifest.manifest_hash(), changed);
    let mut changed = base.clone();
    changed.dataset_version = Version::new(1, 1, 0);
    assert_manifest_hash_change(manifest.manifest_hash(), changed);
    let mut changed = base.clone();
    changed.created_at_ns += 1;
    assert_manifest_hash_change(manifest.manifest_hash(), changed);
    let mut changed = base.clone();
    changed.entity_universe_hash = [2; 32];
    assert_manifest_hash_change(manifest.manifest_hash(), changed);
    let mut changed = base.clone();
    changed
        .source_versions
        .insert("binance".to_owned(), "2026-07-30".to_owned());
    assert_manifest_hash_change(manifest.manifest_hash(), changed);
    let mut changed = base.clone();
    changed
        .schema_versions
        .insert("feature_observation".to_owned(), 4);
    assert_manifest_hash_change(manifest.manifest_hash(), changed);
    let mut changed = base.clone();
    changed
        .feature_versions
        .insert("return_1h".to_owned(), Version::new(2, 0, 0));
    assert_manifest_hash_change(manifest.manifest_hash(), changed);
    let mut changed = base.clone();
    changed
        .label_versions
        .insert("downside_5pct".to_owned(), Version::new(2, 0, 0));
    assert_manifest_hash_change(manifest.manifest_hash(), changed);
    let mut changed = base.clone();
    changed.partition_hashes.push([10; 32]);
    assert_manifest_hash_change(manifest.manifest_hash(), changed);
    let mut changed = base.clone();
    changed
        .exclusion_counts
        .insert("unresolved_correction".to_owned(), 3);
    assert_manifest_hash_change(manifest.manifest_hash(), changed);
    let mut changed = base.clone();
    changed.code_commit = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned();
    assert_manifest_hash_change(manifest.manifest_hash(), changed);
    let mut changed = base.clone();
    changed.license_manifest_hash = [11; 32];
    assert_manifest_hash_change(manifest.manifest_hash(), changed);

    let mut reordered = base.clone();
    reordered.partition_hashes.reverse();
    assert_eq!(
        manifest.manifest_hash(),
        DatasetManifest::try_new(reordered)
            .expect("partition order is canonical")
            .manifest_hash()
    );

    let mut duplicate_partition = base;
    duplicate_partition.partition_hashes = vec![[7; 32], [7; 32]];
    assert_eq!(
        DatasetManifest::try_new(duplicate_partition),
        Err(DatasetError::InvalidManifest)
    );

    let mut noncanonical_commit = manifest_input();
    noncanonical_commit.code_commit = "ABCDEFABCDEFABCDEFABCDEFABCDEFABCDEFABCD".to_owned();
    assert_eq!(
        DatasetManifest::try_new(noncanonical_commit),
        Err(DatasetError::InvalidManifest)
    );
}

#[test]
fn invalid_numeric_feature_and_incomplete_label_fail_closed() {
    let mut nan = join_input();
    nan.features[0].value = Some(f64::NAN);
    assert!(matches!(
        join_point_in_time(nan),
        Err(DatasetError::InvalidFeature { .. })
    ));

    let mut low_quality = join_input();
    low_quality.features[0].quality_score_millionths = 899_999;
    assert!(matches!(
        join_point_in_time(low_quality),
        Err(DatasetError::FeatureQualityBelowRequirement { .. })
    ));

    let mut premature_negative = join_input();
    premature_negative.labels[0].outcome_known_at_ns =
        premature_negative.origin_time_ns + DAY_NS - 1;
    assert!(matches!(
        join_point_in_time(premature_negative),
        Err(DatasetError::InvalidLabel { .. })
    ));

    let mut mixed_label_versions = join_input();
    let mut second_version = mixed_label_versions.labels[0].clone();
    second_version.label_version = Version::new(2, 0, 0);
    mixed_label_versions.labels.push(second_version);
    assert_eq!(
        join_point_in_time(mixed_label_versions),
        Err(DatasetError::DuplicateLabel {
            label_id: "downside_5pct".to_owned(),
        })
    );
}

#[test]
fn embargo_covers_max_horizon_and_publication_lag() {
    let samples = Sample::daily_fixture(500).expect("bounded fixture");
    let builder = FoldBuilder::try_new(86_400, 7_200).expect("valid fold builder");
    let folds = builder.outer_folds(&samples).expect("enough history");
    assert!(!folds.is_empty());
    assert_eq!(
        folds[0].fold_hash(),
        [
            61, 205, 22, 11, 107, 40, 90, 129, 246, 211, 153, 119, 209, 98, 12, 38, 93, 164, 205,
            137, 127, 148, 231, 255, 30, 243, 160, 100, 173, 185, 30, 124,
        ]
    );
    assert_eq!(
        folds[0].inner_folds()[0].fold_hash(),
        [
            192, 93, 120, 10, 207, 75, 61, 250, 15, 204, 22, 82, 160, 70, 39, 134, 85, 178, 188,
            110, 76, 69, 212, 144, 204, 239, 177, 149, 114, 192, 153, 135,
        ]
    );
    let required_gap_ns =
        i64::try_from((86_400_u64 + 7_200) * 1_000_000_000).expect("reference gap fits");
    for fold in &folds {
        assert!(fold.training().end_ns() + required_gap_ns <= fold.calibration().start_ns());
        assert!(fold.calibration().end_ns() + required_gap_ns <= fold.test().start_ns());
        assert!(!fold.inner_folds().is_empty());
        for inner in fold.inner_folds() {
            assert!(inner.training().end_ns() + required_gap_ns <= inner.validation().start_ns());
        }
    }
    assert_eq!(
        folds,
        builder
            .outer_folds(&samples)
            .expect("fold construction is deterministic")
    );

    let custom_folds = FoldBuilder::try_with_schedule(
        86_400,
        7_200,
        WalkForwardSchedule {
            initial_training_seconds: 120 * DAY_SECONDS,
            minimum_inner_training_seconds: 30 * DAY_SECONDS,
            inner_validation_seconds: 15 * DAY_SECONDS,
            calibration_seconds: 15 * DAY_SECONDS,
            test_seconds: 15 * DAY_SECONDS,
            step_seconds: 15 * DAY_SECONDS,
        },
    )
    .expect("valid explicit schedule")
    .outer_folds(&samples)
    .expect("custom schedule has eligible history");
    assert_ne!(folds[0].fold_hash(), custom_folds[0].fold_hash());
}

#[test]
fn fold_roles_are_time_ordered_and_overlapping_outcomes_are_purged() {
    let mut samples = Sample::daily_fixture(500).expect("bounded fixture");
    let folds = FoldBuilder::try_new(DAY_SECONDS, 7_200)
        .expect("valid builder")
        .outer_folds(&samples)
        .expect("folds");
    let fold = &folds[0];

    let leaking_index = samples
        .iter()
        .rposition(|sample| sample.origin_time_ns() < fold.training().end_ns())
        .expect("training sample");
    samples[leaking_index] = samples[leaking_index]
        .with_outcome_end_ns(fold.calibration().start_ns())
        .expect("valid overlapping outcome");

    let training = fold.training_samples(&samples).expect("training selection");
    assert!(
        training
            .iter()
            .all(|sample| sample.outcome_end_ns() <= fold.training().end_ns())
    );
    assert!(
        !training
            .iter()
            .any(|sample| { sample.origin_time_ns() == samples[leaking_index].origin_time_ns() })
    );

    let calibration = fold
        .calibration_samples(&samples)
        .expect("calibration selection");
    let test = fold.test_samples(&samples).expect("test selection");
    assert!(training.iter().all(|train| {
        calibration
            .iter()
            .chain(test.iter())
            .all(|later| train.id() != later.id())
    }));
    assert!(
        calibration
            .iter()
            .all(|value| test.iter().all(|later| value.id() != later.id()))
    );
}

#[test]
fn unsorted_duplicate_or_insufficient_samples_fail_closed() {
    let mut unsorted = Sample::daily_fixture(500).expect("fixture");
    unsorted.swap(0, 1);
    let builder = FoldBuilder::try_new(DAY_SECONDS, 7_200).expect("builder");
    assert_eq!(
        builder.outer_folds(&unsorted),
        Err(DatasetError::NonMonotonicSamples)
    );

    let insufficient = Sample::daily_fixture(10).expect("fixture");
    assert_eq!(
        builder.outer_folds(&insufficient),
        Err(DatasetError::InsufficientHistory)
    );
    assert!(matches!(
        FoldBuilder::try_new(u64::MAX, 1),
        Err(DatasetError::TimeOverflow)
    ));
    assert_eq!(
        FoldBuilder::try_with_schedule(
            DAY_SECONDS,
            0,
            WalkForwardSchedule {
                step_seconds: 0,
                ..WalkForwardSchedule::reference()
            },
        ),
        Err(DatasetError::InvalidFoldConfiguration)
    );
    assert_eq!(
        FoldBuilder::try_with_schedule(
            DAY_SECONDS,
            0,
            WalkForwardSchedule {
                minimum_inner_training_seconds: 180 * DAY_SECONDS,
                ..WalkForwardSchedule::reference()
            },
        ),
        Err(DatasetError::InvalidFoldConfiguration)
    );
    assert_eq!(
        FoldBuilder::try_with_schedule(150 * DAY_SECONDS, 0, WalkForwardSchedule::reference(),),
        Err(DatasetError::InvalidFoldConfiguration)
    );

    let mut duplicate_id = Sample::daily_fixture(500).expect("fixture");
    duplicate_id[2] = Sample::try_new(
        duplicate_id[0].id(),
        duplicate_id[2].origin_time_ns(),
        duplicate_id[2].outcome_end_ns(),
        duplicate_id[2].as_known_at_ns(),
    )
    .expect("valid duplicate-id sample");
    assert_eq!(
        builder.outer_folds(&duplicate_id),
        Err(DatasetError::NonMonotonicSamples)
    );

    let sparse = vec![
        Sample::try_new(1, DAY_NS, DAY_NS, DAY_NS).expect("first sparse sample"),
        Sample::try_new(2, 500 * DAY_NS, 500 * DAY_NS, 500 * DAY_NS).expect("last sparse sample"),
    ];
    assert_eq!(
        builder.outer_folds(&sparse),
        Err(DatasetError::InsufficientHistory)
    );
}

fn join_input() -> JoinInput {
    let origin_time_ns = 100 * DAY_NS;
    JoinInput {
        asset: "btc".to_owned(),
        instrument_generation: 1,
        origin_time_ns,
        dataset_cutoff_ns: origin_time_ns + 2 * DAY_NS,
        universe: UniverseMembership {
            valid_from_ns: DAY_NS,
            valid_to_ns: None,
            as_known_at_ns: origin_time_ns - DAY_NS,
            revision: 1,
            instrument_definition_hash: [2; 32],
            lineage_hash: [3; 32],
        },
        features: vec![FeatureValue {
            feature_id: "return_1h".to_owned(),
            feature_version: Version::new(1, 0, 0),
            value: Some(-0.01),
            event_time_end_ns: origin_time_ns,
            as_known_at_ns: origin_time_ns,
            quality_score_millionths: 950_000,
            revision: 1,
            lineage_hash: [4; 32],
        }],
        labels: vec![EventLabel {
            label_id: "downside_5pct".to_owned(),
            label_version: Version::new(1, 0, 0),
            definition_hash: [5; 32],
            origin_time_ns,
            horizon_seconds: DAY_SECONDS,
            outcome: LabelOutcome::NotOccurred,
            outcome_known_at_ns: origin_time_ns + DAY_NS,
            revision: 1,
            source_range_hash: [6; 32],
        }],
        minimum_quality_millionths: 900_000,
    }
}

fn manifest_input() -> DatasetManifestInput {
    DatasetManifestInput {
        dataset_id: "btc_transition_training".to_owned(),
        dataset_version: Version::new(1, 0, 0),
        created_at_ns: 600 * DAY_NS,
        as_known_at_cutoff_ns: 599 * DAY_NS,
        entity_universe_hash: [1; 32],
        source_versions: BTreeMap::from([("binance".to_owned(), "2026-07-29".to_owned())]),
        schema_versions: BTreeMap::from([("feature_observation".to_owned(), 3)]),
        feature_versions: BTreeMap::from([("return_1h".to_owned(), Version::new(1, 0, 0))]),
        label_versions: BTreeMap::from([("downside_5pct".to_owned(), Version::new(1, 0, 0))]),
        partition_hashes: vec![[7; 32], [8; 32]],
        exclusion_counts: BTreeMap::from([("unresolved_correction".to_owned(), 2)]),
        correction_policy: CorrectionPolicy::AsKnownAtCutoff,
        code_commit: "90e1264a367d53efc7fdbbd34966d2ee834eb22c".to_owned(),
        license_manifest_hash: [9; 32],
    }
}

fn assert_manifest_hash_change(base_hash: [u8; 32], changed: DatasetManifestInput) {
    assert_ne!(
        base_hash,
        DatasetManifest::try_new(changed)
            .expect("changed manifest")
            .manifest_hash()
    );
}
