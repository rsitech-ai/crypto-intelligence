use std::collections::BTreeMap;

use cusp::ablation::{
    ABLATION_REPORT_SCHEMA_VERSION, AblationReport, AblationReportInput, CuspGate, FoldMetricInput,
    RequiredBaseline,
};
use dataset::{FoldBuilder, OuterFold, Sample};
use domain::{AssetId, AssetNamespace, UnixNanos};
use ed25519_dalek::SigningKey;
use ensemble::{
    CuspEligibilityReceipt, EnsembleError, EnsembleSchema, MatrixColumn, MatrixDatum, MatrixLimits,
    MatrixModuleState, ModuleAvailability, ModuleDatum, ModuleKind, ModuleOutput,
    ModuleOutputInput, OutOfFoldMatrix, RowModule, TrainingRowInput, VerifiedModuleOutputInput,
};
use feature_registry::{FeatureEntity, FeatureId, FiniteF64, MissingnessReason, QualityScore};
use model_registry::{
    CalibrationDescriptor, MetricSummary, ModelPackageManifestInput, PackagePeriod, PackageSigner,
    QualityRequirements, RuntimeRequirements, TrustedVerifyingKey, VerifiedPackage, verify_package,
};
use semver::Version;

const DAY_SECONDS: u64 = 86_400;

fn fold_fixture() -> (OuterFold, OuterFold, Vec<Sample>) {
    let samples = Sample::daily_fixture(420).expect("daily samples");
    let folds = FoldBuilder::try_new(DAY_SECONDS, 0)
        .expect("fold builder")
        .outer_folds(&samples)
        .expect("outer folds");
    (folds[0].clone(), folds[1].clone(), samples)
}

fn test_samples(fold: &OuterFold, samples: &[Sample]) -> Vec<Sample> {
    fold.test_samples(samples)
        .expect("test samples")
        .into_iter()
        .copied()
        .collect()
}

fn btc() -> FeatureEntity {
    FeatureEntity::Asset(
        AssetId::new(AssetNamespace::Native, "bitcoin", "", "BTC", 1).expect("BTC asset"),
    )
}

fn eth() -> FeatureEntity {
    FeatureEntity::Asset(
        AssetId::new(AssetNamespace::Native, "ethereum", "", "ETH", 1).expect("ETH asset"),
    )
}

fn feature(id: &str) -> FeatureId {
    FeatureId::new(id).expect("canonical feature ID")
}

fn present(value: f64) -> ModuleDatum {
    ModuleDatum::Present(FiniteF64::new(value).expect("finite value"))
}

fn values(entries: &[(&str, ModuleDatum)]) -> BTreeMap<FeatureId, ModuleDatum> {
    entries
        .iter()
        .map(|(id, datum)| (feature(id), datum.clone()))
        .collect()
}

fn output(
    kind: ModuleKind,
    sample: Sample,
    trained_through_ns: i64,
    fold_hash: [u8; 32],
    availability: ModuleAvailability,
    values: BTreeMap<FeatureId, ModuleDatum>,
    evidence_byte: u8,
) -> ModuleOutput {
    ModuleOutput::try_new(output_input(
        kind,
        sample,
        trained_through_ns,
        fold_hash,
        availability,
        values,
        evidence_byte,
    ))
    .expect("valid module output")
}

fn output_input(
    kind: ModuleKind,
    sample: Sample,
    trained_through_ns: i64,
    fold_hash: [u8; 32],
    availability: ModuleAvailability,
    values: BTreeMap<FeatureId, ModuleDatum>,
    evidence_byte: u8,
) -> ModuleOutputInput {
    ModuleOutputInput {
        module_kind: kind,
        model_id: format!("{}-model-v1", kind.as_str()),
        model_package_hash: module_package_hash(kind),
        entity: btc(),
        origin: UnixNanos::new(sample.origin_time_ns()),
        as_known_at: UnixNanos::new(sample.origin_time_ns()),
        trained_through: UnixNanos::new(trained_through_ns),
        outer_fold_hash: fold_hash,
        values,
        availability,
        quality: QualityScore::from_millionths(900_000).expect("quality"),
        source_evidence_hash: [evidence_byte; 32],
    }
}

fn output_with_package_hash(
    mut input: ModuleOutputInput,
    model_package_hash: [u8; 32],
) -> ModuleOutput {
    input.model_package_hash = model_package_hash;
    ModuleOutput::try_new(input).expect("valid module output")
}

fn module_package_hash(kind: ModuleKind) -> [u8; 32] {
    let byte = match kind {
        ModuleKind::BaseRate => 0x81,
        ModuleKind::Volatility => 0x82,
        ModuleKind::Changepoint => 0x83,
        ModuleKind::Regime => 0x84,
        ModuleKind::Hazard => 0x85,
        ModuleKind::Cusp => 0x86,
    };
    [byte; 32]
}

fn schema(columns: &[(ModuleKind, &str)]) -> EnsembleSchema {
    EnsembleSchema::try_new(
        1,
        columns
            .iter()
            .map(|(kind, id)| MatrixColumn::new(*kind, feature(id)))
            .collect(),
    )
    .expect("valid schema")
}

fn row(sample: Sample, entity: FeatureEntity, modules: Vec<RowModule>) -> TrainingRowInput {
    TrainingRowInput {
        sample,
        entity,
        modules,
    }
}

fn locked() -> CuspEligibilityReceipt {
    CuspEligibilityReceipt::locked_without_gate()
}

fn eligible_cusp_receipt(candidate_model_hash: [u8; 32]) -> CuspEligibilityReceipt {
    let passing_fold = |index: i64, regime_id: &str| FoldMetricInput {
        fold_id: format!("outer-{index}"),
        regime_id: regime_id.to_owned(),
        test_start_ns: 1_800_000_000_000_000_000 + index * 1_000_000_000,
        test_end_ns: 1_800_000_000_500_000_000 + index * 1_000_000_000,
        observation_count: 1_000,
        positive_event_count: 10,
        independent_event_episode_count: 2,
        baseline_brier_score: 0.20,
        candidate_brier_score: 0.15,
        calibration_slope: 1.0,
        calibration_intercept: 0.01,
        expected_calibration_error: 0.02,
        alert_budget_score_delta: 0.03,
    };
    let report = AblationReport::try_new(AblationReportInput {
        schema_version: ABLATION_REPORT_SCHEMA_VERSION,
        dataset_manifest_hash: [0x11; 32],
        candidate_model_hash,
        baseline_model_hash: [0x33; 32],
        required_baselines: vec![
            RequiredBaseline::Volatility,
            RequiredBaseline::Leverage,
            RequiredBaseline::Regime,
            RequiredBaseline::Microstructure,
        ],
        folds: vec![
            passing_fold(1, "low-volatility"),
            passing_fold(2, "high-volatility"),
            passing_fold(3, "liquidity-stress"),
            passing_fold(4, "low-volatility"),
            passing_fold(5, "high-volatility"),
        ],
        maximum_standardized_coefficient_drift: 0.75,
        sign_scaling_parity: true,
        numerical_failures: 0,
        attempted_model_variants: 3,
    })
    .expect("valid passing ablation report");
    let evaluation = CuspGate::default()
        .evaluate(&report)
        .expect("valid passing gate evaluation");
    CuspEligibilityReceipt::from(&evaluation)
}

fn verified_package(fold: &OuterFold) -> VerifiedPackage {
    const ARTIFACT: &[u8] = b"ensemble-module-fixture-v1";
    let test = fold.test();
    let validation_end = test.start_ns() - 2;
    let validation_start = validation_end - 10;
    let training_end = validation_start - 2;
    let training_start = training_end - 10;
    let validation_period =
        PackagePeriod::new(validation_start, validation_end).expect("validation period");
    let manifest = ModelPackageManifestInput {
        schema_version: 1,
        model_id: "verified-hazard-v1".into(),
        semantic_version: Version::new(1, 0, 0),
        model_family: "competing-risk-hazard".into(),
        supported_assets: vec!["BTC".into()],
        supported_event_types: vec!["drawdown".into()],
        supported_horizons_seconds: vec![900],
        training_period: PackagePeriod::new(training_start, training_end).expect("training period"),
        validation_period,
        outer_test_period: PackagePeriod::new(test.start_ns(), test.end_ns()).expect("test period"),
        data_hash: [1; 32],
        feature_schema_hash: [2; 32],
        normalization_hash: [3; 32],
        label_definition_hash: [4; 32],
        code_commit: "0123456789abcdef0123456789abcdef01234567".into(),
        hyperparameters_blake3: [5; 32],
        calibration: CalibrationDescriptor {
            method: "beta".into(),
            version: "beta-monotone-v1".into(),
            evidence_blake3: [6; 32],
            validation_period,
            effective_sample_size: 100,
        },
        quality_requirements: QualityRequirements {
            minimum_quality_millionths: 900_000,
            minimum_source_count: 2,
            minimum_outer_test_positives: 10,
            minimum_shadow_positives: 5,
            minimum_brier_skill_score: 0.05,
            maximum_log_loss: 0.5,
            calibration_slope_minimum: 0.8,
            calibration_slope_maximum: 1.2,
            calibration_intercept_absolute_maximum: 0.05,
        },
        runtime_requirements: RuntimeRequirements {
            minimum_runtime_version: Version::new(1, 0, 0),
            required_capabilities: vec!["hazard-v1".into()],
            minimum_memory_bytes: 1_024,
        },
        metrics: MetricSummary {
            outer_test_evidence_blake3: [7; 32],
            observations: 200,
            positives: 25,
            log_loss: 0.22,
            brier_skill_score: 0.12,
            calibration_intercept: 0.01,
            calibration_slope: 0.98,
        },
        subgroup_metrics_blake3: [8; 32],
        limitations: vec!["fixture evidence is not production proof".into()],
        model_card_blake3: [9; 32],
        review_after_ns: test.end_ns() + 100,
        expires_at_ns: test.end_ns() + 200,
    };
    let signing_key = SigningKey::from_bytes(&[0x42; 32]);
    let trusted_key = TrustedVerifyingKey::new("fixture-key-v1", signing_key.verifying_key())
        .expect("trusted key");
    let signed = PackageSigner::sign(manifest, ARTIFACT, "fixture-key-v1", &signing_key)
        .expect("signed fixture");
    verify_package(&signed, ARTIFACT, &trusted_key).expect("verified fixture")
}

#[test]
fn in_fold_prediction_is_rejected_from_stacker_training() {
    let (fold, _, samples) = fold_fixture();
    let sample = test_samples(&fold, &samples)[1];
    let leaked = output(
        ModuleKind::Hazard,
        sample,
        fold.test().start_ns(),
        fold.fold_hash(),
        ModuleAvailability::Available,
        values(&[("hazard_downside_probability", present(0.4))]),
        1,
    );
    let schema = schema(&[(ModuleKind::Hazard, "hazard_downside_probability")]);

    assert_eq!(
        OutOfFoldMatrix::build(
            &schema,
            &fold,
            vec![row(sample, btc(), vec![RowModule::output(leaked)])],
            locked(),
        ),
        Err(EnsembleError::InFoldPrediction)
    );
}

#[test]
fn research_cusp_column_is_present_but_weight_locked_independently_of_availability() {
    let (fold, _, samples) = fold_fixture();
    let sample = test_samples(&fold, &samples)[0];
    let cusp = output(
        ModuleKind::Cusp,
        sample,
        fold.test().start_ns() - 1,
        fold.fold_hash(),
        ModuleAvailability::Available,
        values(&[("cusp_region_probability", present(0.7))]),
        2,
    );
    let schema = schema(&[(ModuleKind::Cusp, "cusp_region_probability")]);
    let matrix = OutOfFoldMatrix::build(
        &schema,
        &fold,
        vec![row(sample, btc(), vec![RowModule::output(cusp)])],
        locked(),
    )
    .expect("research-only cusp matrix");

    assert_eq!(matrix.columns()[0].id(), "cusp_region_probability");
    assert!(
        matrix
            .constraints()
            .is_weight_locked("cusp_region_probability")
    );
    assert_eq!(
        matrix.rows()[0].value("cusp_region_probability"),
        Some(&MatrixDatum::Present(FiniteF64::new(0.7).expect("finite")))
    );
    assert_eq!(
        matrix.rows()[0].module_state(ModuleKind::Cusp),
        Some(MatrixModuleState::Reported(ModuleAvailability::Available))
    );
}

#[test]
fn eligible_cusp_receipt_unlocks_only_its_exact_candidate_package() {
    let (fold, _, samples) = fold_fixture();
    let sample = test_samples(&fold, &samples)[0];
    let candidate_hash = [0x22; 32];
    let receipt = eligible_cusp_receipt(candidate_hash);
    let schema = schema(&[(ModuleKind::Cusp, "cusp_region_probability")]);
    let make_output = |package_hash| {
        output_with_package_hash(
            output_input(
                ModuleKind::Cusp,
                sample,
                fold.test().start_ns() - 1,
                fold.fold_hash(),
                ModuleAvailability::Available,
                values(&[("cusp_region_probability", present(0.7))]),
                2,
            ),
            package_hash,
        )
    };

    assert_eq!(receipt.candidate_model_hash(), candidate_hash);
    assert_eq!(
        OutOfFoldMatrix::build(
            &schema,
            &fold,
            vec![row(
                sample,
                btc(),
                vec![RowModule::output(make_output([0x23; 32]))],
            )],
            receipt,
        ),
        Err(EnsembleError::CuspCandidateMismatch)
    );

    let matrix = OutOfFoldMatrix::build(
        &schema,
        &fold,
        vec![row(
            sample,
            btc(),
            vec![RowModule::output(make_output(candidate_hash))],
        )],
        receipt,
    )
    .expect("matching eligible cusp package");
    assert!(
        !matrix
            .constraints()
            .is_weight_locked("cusp_region_probability")
    );
}

#[test]
fn eligible_cusp_receipt_cannot_unlock_an_entirely_missing_candidate() {
    let (fold, _, samples) = fold_fixture();
    let sample = test_samples(&fold, &samples)[0];
    let schema = schema(&[(ModuleKind::Cusp, "cusp_region_probability")]);

    assert_eq!(
        OutOfFoldMatrix::build(
            &schema,
            &fold,
            vec![row(
                sample,
                btc(),
                vec![RowModule::Missing {
                    module_kind: ModuleKind::Cusp,
                    availability: ModuleAvailability::Unavailable(
                        MissingnessReason::InsufficientHistory,
                    ),
                }],
            )],
            eligible_cusp_receipt([0x22; 32]),
        ),
        Err(EnsembleError::CuspCandidateMismatch)
    );
}

#[test]
fn entirely_absent_declared_module_remains_reasoned_and_never_becomes_zero() {
    let (fold, _, samples) = fold_fixture();
    let sample = test_samples(&fold, &samples)[0];
    let hazard = output(
        ModuleKind::Hazard,
        sample,
        fold.test().start_ns() - 1,
        fold.fold_hash(),
        ModuleAvailability::Available,
        values(&[("hazard_downside_probability", present(0.3))]),
        3,
    );
    let schema = schema(&[
        (ModuleKind::Volatility, "volatility_forecast"),
        (ModuleKind::Hazard, "hazard_downside_probability"),
        (ModuleKind::Cusp, "cusp_region_probability"),
    ]);
    let missing = ModuleAvailability::Unavailable(MissingnessReason::InsufficientHistory);
    let matrix = OutOfFoldMatrix::build(
        &schema,
        &fold,
        vec![row(
            sample,
            btc(),
            vec![
                RowModule::Missing {
                    module_kind: ModuleKind::Volatility,
                    availability: missing,
                },
                RowModule::output(hazard),
                RowModule::Missing {
                    module_kind: ModuleKind::Cusp,
                    availability: ModuleAvailability::Abstained(
                        MissingnessReason::ModelNotApplicable,
                    ),
                },
            ],
        )],
        locked(),
    )
    .expect("explicit missing modules");

    assert_eq!(
        matrix.rows()[0].value("volatility_forecast"),
        Some(&MatrixDatum::ModuleMissing(
            MissingnessReason::InsufficientHistory
        ))
    );
    assert_eq!(
        matrix.rows()[0].module_state(ModuleKind::Volatility),
        Some(MatrixModuleState::Missing(missing))
    );
    assert_eq!(
        matrix.rows()[0].value("cusp_region_probability"),
        Some(&MatrixDatum::ModuleMissing(
            MissingnessReason::ModelNotApplicable
        ))
    );
    assert!(
        matrix
            .constraints()
            .is_weight_locked("cusp_region_probability")
    );
}

#[test]
fn fold_entity_origin_schema_and_duplicate_mutations_fail_closed() {
    let (fold, other_fold, samples) = fold_fixture();
    let selected = test_samples(&fold, &samples);
    let first_sample = selected[0];
    let second_sample = selected[1];
    let declared = schema(&[(ModuleKind::Hazard, "hazard_downside_probability")]);
    let make = |sample, fold_hash, id, feature_id| {
        output(
            ModuleKind::Hazard,
            sample,
            fold.test().start_ns() - 1,
            fold_hash,
            ModuleAvailability::Available,
            values(&[(feature_id, present(0.3))]),
            id,
        )
    };

    let wrong_fold = make(
        first_sample,
        other_fold.fold_hash(),
        4,
        "hazard_downside_probability",
    );
    assert_eq!(
        OutOfFoldMatrix::build(
            &declared,
            &fold,
            vec![row(
                first_sample,
                btc(),
                vec![RowModule::output(wrong_fold)]
            )],
            locked(),
        ),
        Err(EnsembleError::FoldMismatch)
    );

    let wrong_entity = make(
        first_sample,
        fold.fold_hash(),
        5,
        "hazard_downside_probability",
    );
    assert_eq!(
        OutOfFoldMatrix::build(
            &declared,
            &fold,
            vec![row(
                first_sample,
                eth(),
                vec![RowModule::output(wrong_entity)]
            )],
            locked(),
        ),
        Err(EnsembleError::EntityMismatch)
    );

    let wrong_origin = make(
        first_sample,
        fold.fold_hash(),
        6,
        "hazard_downside_probability",
    );
    assert_eq!(
        OutOfFoldMatrix::build(
            &declared,
            &fold,
            vec![row(
                second_sample,
                btc(),
                vec![RowModule::output(wrong_origin)]
            )],
            locked(),
        ),
        Err(EnsembleError::OriginMismatch)
    );

    let duplicate = make(
        first_sample,
        fold.fold_hash(),
        7,
        "hazard_downside_probability",
    );
    assert_eq!(
        OutOfFoldMatrix::build(
            &declared,
            &fold,
            vec![row(
                first_sample,
                btc(),
                vec![
                    RowModule::output(duplicate.clone()),
                    RowModule::output(duplicate)
                ],
            )],
            locked(),
        ),
        Err(EnsembleError::DuplicateModule)
    );

    let drifted = make(first_sample, fold.fold_hash(), 8, "renamed_hazard_risk");
    assert_eq!(
        OutOfFoldMatrix::build(
            &declared,
            &fold,
            vec![row(first_sample, btc(), vec![RowModule::output(drifted)])],
            locked(),
        ),
        Err(EnsembleError::SchemaMismatch)
    );
}

#[test]
fn validation_precedence_is_independent_of_duplicate_module_order() {
    let (fold, other_fold, samples) = fold_fixture();
    let sample = test_samples(&fold, &samples)[0];
    let declared = schema(&[(ModuleKind::Hazard, "hazard_downside_probability")]);
    let make = |fold_hash, evidence_byte| {
        output(
            ModuleKind::Hazard,
            sample,
            fold.test().start_ns() - 1,
            fold_hash,
            ModuleAvailability::Available,
            values(&[("hazard_downside_probability", present(0.3))]),
            evidence_byte,
        )
    };
    let valid = make(fold.fold_hash(), 31);
    let invalid = make(other_fold.fold_hash(), 32);

    for modules in [
        vec![
            RowModule::output(valid.clone()),
            RowModule::output(invalid.clone()),
        ],
        vec![
            RowModule::output(invalid.clone()),
            RowModule::output(valid.clone()),
        ],
    ] {
        assert_eq!(
            OutOfFoldMatrix::build(
                &declared,
                &fold,
                vec![row(sample, btc(), modules)],
                locked(),
            ),
            Err(EnsembleError::DuplicateModule)
        );
    }
}

#[test]
fn one_outer_fold_rejects_mixed_module_package_identities() {
    let (fold, _, samples) = fold_fixture();
    let selected = test_samples(&fold, &samples);
    let declared = schema(&[(ModuleKind::Hazard, "hazard_downside_probability")]);
    let make_row = |sample, package_hash, evidence_byte| {
        row(
            sample,
            btc(),
            vec![RowModule::output(output_with_package_hash(
                output_input(
                    ModuleKind::Hazard,
                    sample,
                    fold.test().start_ns() - 1,
                    fold.fold_hash(),
                    ModuleAvailability::Available,
                    values(&[("hazard_downside_probability", present(0.3))]),
                    evidence_byte,
                ),
                package_hash,
            ))],
        )
    };

    assert_eq!(
        OutOfFoldMatrix::build(
            &declared,
            &fold,
            vec![
                make_row(selected[0], [0x40; 32], 33),
                make_row(selected[1], [0x41; 32], 34),
            ],
            locked(),
        ),
        Err(EnsembleError::ModulePackageMismatch)
    );
}

#[test]
fn exact_module_coverage_and_absence_states_are_enforced() {
    let (fold, _, samples) = fold_fixture();
    let sample = test_samples(&fold, &samples)[0];
    let schema = schema(&[
        (ModuleKind::Hazard, "hazard_downside_probability"),
        (ModuleKind::Cusp, "cusp_region_probability"),
    ]);
    let hazard = output(
        ModuleKind::Hazard,
        sample,
        fold.test().start_ns() - 1,
        fold.fold_hash(),
        ModuleAvailability::Available,
        values(&[("hazard_downside_probability", present(0.3))]),
        9,
    );

    assert_eq!(
        OutOfFoldMatrix::build(
            &schema,
            &fold,
            vec![row(sample, btc(), vec![RowModule::output(hazard.clone())])],
            locked(),
        ),
        Err(EnsembleError::ModuleCoverageMismatch)
    );
    assert_eq!(
        OutOfFoldMatrix::build(
            &schema,
            &fold,
            vec![row(
                sample,
                btc(),
                vec![
                    RowModule::output(hazard),
                    RowModule::Missing {
                        module_kind: ModuleKind::Cusp,
                        availability: ModuleAvailability::Available,
                    },
                ],
            )],
            locked(),
        ),
        Err(EnsembleError::InvalidMissingModule)
    );
}

#[test]
fn matrix_identity_is_input_order_independent_and_semantic_mutations_are_visible() {
    let (fold, _, samples) = fold_fixture();
    let selected = test_samples(&fold, &samples);
    let first_sample = selected[0];
    let second_sample = selected[1];
    let schema = schema(&[(ModuleKind::Hazard, "hazard_downside_probability")]);
    let make_row = |sample, probability, reason_byte| {
        row(
            sample,
            btc(),
            vec![RowModule::output(output(
                ModuleKind::Hazard,
                sample,
                fold.test().start_ns() - 1,
                fold.fold_hash(),
                ModuleAvailability::Available,
                values(&[("hazard_downside_probability", present(probability))]),
                reason_byte,
            ))],
        )
    };
    let canonical = OutOfFoldMatrix::build(
        &schema,
        &fold,
        vec![
            make_row(first_sample, 0.3, 10),
            make_row(second_sample, 0.4, 11),
        ],
        locked(),
    )
    .expect("canonical matrix");
    let permuted = OutOfFoldMatrix::build(
        &schema,
        &fold,
        vec![
            make_row(second_sample, 0.4, 11),
            make_row(first_sample, 0.3, 10),
        ],
        locked(),
    )
    .expect("permuted matrix");
    let mutated = OutOfFoldMatrix::build(
        &schema,
        &fold,
        vec![make_row(first_sample, 0.31, 10)],
        locked(),
    )
    .expect("mutated matrix");

    assert_eq!(canonical.evidence_hash(), permuted.evidence_hash());
    assert_ne!(canonical.evidence_hash(), mutated.evidence_hash());
    assert!(canonical.evidence_hash().iter().any(|byte| *byte != 0));
    assert_eq!(canonical.rows()[0].row_id(), first_sample.id());
}

#[test]
fn schema_order_is_canonical_and_missingness_changes_matrix_identity() {
    let first_schema = schema(&[
        (ModuleKind::Cusp, "cusp_region_probability"),
        (ModuleKind::Hazard, "hazard_downside_probability"),
    ]);
    let reversed_schema = schema(&[
        (ModuleKind::Hazard, "hazard_downside_probability"),
        (ModuleKind::Cusp, "cusp_region_probability"),
    ]);
    assert_eq!(
        first_schema.evidence_hash(),
        reversed_schema.evidence_hash()
    );
    assert_eq!(first_schema.columns(), reversed_schema.columns());

    let (fold, _, samples) = fold_fixture();
    let sample = test_samples(&fold, &samples)[0];
    let cusp_only = schema(&[(ModuleKind::Cusp, "cusp_region_probability")]);
    let missing_row = |reason| {
        row(
            sample,
            btc(),
            vec![RowModule::Missing {
                module_kind: ModuleKind::Cusp,
                availability: ModuleAvailability::Unavailable(reason),
            }],
        )
    };
    let stale = OutOfFoldMatrix::build(
        &cusp_only,
        &fold,
        vec![missing_row(MissingnessReason::Stale)],
        locked(),
    )
    .expect("stale matrix");
    let disconnected = OutOfFoldMatrix::build(
        &cusp_only,
        &fold,
        vec![missing_row(MissingnessReason::SourceDisconnected)],
        locked(),
    )
    .expect("disconnected matrix");
    assert_ne!(stale.evidence_hash(), disconnected.evidence_hash());

    let duplicate = missing_row(MissingnessReason::Stale);
    assert_eq!(
        OutOfFoldMatrix::build(
            &cusp_only,
            &fold,
            vec![duplicate.clone(), duplicate],
            locked(),
        ),
        Err(EnsembleError::DuplicateRow)
    );
}

#[test]
fn module_output_rejects_future_known_zero_digest_and_availability_contradictions() {
    let (fold, _, samples) = fold_fixture();
    let sample = test_samples(&fold, &samples)[0];
    let base = ModuleOutputInput {
        module_kind: ModuleKind::Hazard,
        model_id: "hazard-model-v1".to_owned(),
        model_package_hash: [12; 32],
        entity: btc(),
        origin: UnixNanos::new(sample.origin_time_ns()),
        as_known_at: UnixNanos::new(sample.origin_time_ns()),
        trained_through: UnixNanos::new(fold.test().start_ns() - 1),
        outer_fold_hash: fold.fold_hash(),
        values: values(&[("hazard_downside_probability", present(0.3))]),
        availability: ModuleAvailability::Available,
        quality: QualityScore::from_millionths(900_000).expect("quality"),
        source_evidence_hash: [13; 32],
    };

    let mut invalid = base.clone();
    invalid.as_known_at = UnixNanos::new(sample.origin_time_ns() + 1);
    assert_eq!(
        ModuleOutput::try_new(invalid),
        Err(EnsembleError::FutureKnownOutput)
    );

    let mut invalid = base.clone();
    invalid.as_known_at = UnixNanos::new(invalid.trained_through.value() - 1);
    assert_eq!(
        ModuleOutput::try_new(invalid),
        Err(EnsembleError::InvalidModuleOutput)
    );

    let mut exact_training_boundary = base.clone();
    exact_training_boundary.as_known_at = exact_training_boundary.trained_through;
    ModuleOutput::try_new(exact_training_boundary).expect("inclusive training knowledge boundary");

    let mut invalid = base.clone();
    invalid.model_package_hash = [0; 32];
    assert_eq!(
        ModuleOutput::try_new(invalid),
        Err(EnsembleError::ZeroDigest)
    );

    let mut invalid = base;
    invalid.availability = ModuleAvailability::Unavailable(MissingnessReason::SourceDisconnected);
    assert_eq!(
        ModuleOutput::try_new(invalid),
        Err(EnsembleError::AvailabilityContradiction)
    );

    assert!(FiniteF64::new(f64::NAN).is_err());
    assert_eq!(
        FiniteF64::new(-0.0).expect("canonical zero"),
        FiniteF64::new(0.0).expect("canonical zero")
    );
}

#[test]
fn verified_package_adapter_derives_identity_and_binds_training_period() {
    let (fold, _, samples) = fold_fixture();
    let sample = test_samples(&fold, &samples)[0];
    let package = verified_package(&fold);
    let training_end = package.package().manifest.training_period.end_ns;
    let input = VerifiedModuleOutputInput {
        module_kind: ModuleKind::Hazard,
        entity: btc(),
        origin: UnixNanos::new(sample.origin_time_ns()),
        as_known_at: UnixNanos::new(sample.origin_time_ns()),
        trained_through: UnixNanos::new(training_end),
        values: values(&[("hazard_downside_probability", present(0.3))]),
        availability: ModuleAvailability::Available,
        quality: QualityScore::from_millionths(900_000).expect("quality"),
        source_evidence_hash: [18; 32],
    };
    let output = ModuleOutput::try_from_verified_package(input.clone(), &package, &fold)
        .expect("verified model identity");

    assert_eq!(output.model_id(), package.model_id());
    assert_eq!(output.model_package_hash(), package.package_blake3());

    let mut forged_cutoff = input.clone();
    forged_cutoff.trained_through = UnixNanos::new(training_end + 1);
    assert_eq!(
        ModuleOutput::try_from_verified_package(forged_cutoff, &package, &fold),
        Err(EnsembleError::InvalidModelPackage)
    );

    let (_, other_fold, _) = fold_fixture();
    assert_eq!(
        ModuleOutput::try_from_verified_package(input, &package, &other_fold),
        Err(EnsembleError::InvalidModelPackage)
    );
}

#[test]
fn schema_boundaries_row_boundaries_and_work_limits_fail_closed() {
    let duplicate = MatrixColumn::new(ModuleKind::Hazard, feature("hazard_risk"));
    assert_eq!(
        EnsembleSchema::try_new(1, vec![duplicate.clone(), duplicate]),
        Err(EnsembleError::DuplicateColumn)
    );
    assert_eq!(
        EnsembleSchema::try_new(
            2,
            vec![MatrixColumn::new(
                ModuleKind::Hazard,
                feature("hazard_risk")
            )]
        ),
        Err(EnsembleError::UnsupportedSchema)
    );
    assert_eq!(
        MatrixLimits::try_new(0, 1, 1),
        Err(EnsembleError::InvalidLimits)
    );

    let (fold, _, _) = fold_fixture();
    let schema = schema(&[(ModuleKind::Hazard, "hazard_risk")]);
    let at_end = Sample::try_new(
        999_001,
        fold.test().end_ns(),
        fold.test().end_ns(),
        fold.test().end_ns(),
    )
    .expect("valid sample outside fold");
    let outside_output = output(
        ModuleKind::Hazard,
        at_end,
        fold.test().start_ns() - 1,
        fold.fold_hash(),
        ModuleAvailability::Available,
        values(&[("hazard_risk", present(0.3))]),
        14,
    );
    assert_eq!(
        OutOfFoldMatrix::build(
            &schema,
            &fold,
            vec![row(at_end, btc(), vec![RowModule::output(outside_output)])],
            locked(),
        ),
        Err(EnsembleError::InvalidRow)
    );

    let at_start = Sample::try_new(
        999_002,
        fold.test().start_ns(),
        fold.test().start_ns(),
        fold.test().start_ns(),
    )
    .expect("valid boundary sample");
    let start_output = output(
        ModuleKind::Hazard,
        at_start,
        fold.test().start_ns() - 1,
        fold.fold_hash(),
        ModuleAvailability::Available,
        values(&[("hazard_risk", present(0.3))]),
        15,
    );
    OutOfFoldMatrix::build(
        &schema,
        &fold,
        vec![row(at_start, btc(), vec![RowModule::output(start_output)])],
        locked(),
    )
    .expect("inclusive test start");

    let selected = Sample::try_new(
        999_003,
        fold.test().start_ns() + 1,
        fold.test().start_ns() + 1,
        fold.test().start_ns() + 1,
    )
    .expect("second bounded sample");
    let make = |sample, byte| {
        row(
            sample,
            btc(),
            vec![RowModule::output(output(
                ModuleKind::Hazard,
                sample,
                fold.test().start_ns() - 1,
                fold.fold_hash(),
                ModuleAvailability::Available,
                values(&[("hazard_risk", present(0.3))]),
                byte,
            ))],
        )
    };
    assert_eq!(
        OutOfFoldMatrix::build_with_limits(
            &schema,
            &fold,
            vec![make(at_start, 16), make(selected, 17)],
            locked(),
            MatrixLimits::try_new(2, 1, 1).expect("lowered limits"),
        ),
        Err(EnsembleError::CellCapacity)
    );
}
