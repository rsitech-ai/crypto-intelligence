use std::collections::BTreeMap;

use dataset::{FoldBuilder, OuterFold, Sample};
use domain::{AssetId, AssetNamespace, UnixNanos};
use ensemble::{
    AblationStatus, CuspEligibilityReceipt, EnsembleError, EnsembleSchema, MatrixColumn,
    MissingValuePolicy, ModuleAblationReport, ModuleAvailability, ModuleDatum, ModuleKind,
    ModuleOutput, ModuleOutputInput, ModuleVectorInput, OutOfFoldMatrix, RowModule, StackerConfig,
    StackerConfigInput, StackerFitOutcome, StackerFold, StackerTargetInput, StackerTargetSchema,
    StackerTargetSet, StackerTrainingSet, TrainingRowInput, WeightConstraint,
};
use feature_registry::{FeatureEntity, FeatureId, FiniteF64, MissingnessReason, QualityScore};
use labels::{
    DefinitionScope, LabelDefinition, LabelDefinitionInput, LabelOutcome, LabelRule, OverlapPolicy,
    PriceThreshold,
};
use semver::Version;

const DAY_SECONDS: u64 = 86_400;

fn folds_fixture() -> (Vec<OuterFold>, Vec<Sample>) {
    let samples = Sample::daily_fixture(480).expect("daily samples");
    let folds = FoldBuilder::try_new(DAY_SECONDS, 0)
        .expect("fold builder")
        .outer_folds(&samples)
        .expect("outer folds");
    (folds, samples)
}

fn btc() -> FeatureEntity {
    FeatureEntity::Asset(
        AssetId::new(AssetNamespace::Native, "bitcoin", "", "BTC", 1).expect("BTC asset"),
    )
}

fn feature(id: &str) -> FeatureId {
    FeatureId::new(id).expect("canonical feature ID")
}

fn value(number: f64) -> ModuleDatum {
    ModuleDatum::Present(FiniteF64::new(number).expect("finite fixture"))
}

fn output(
    kind: ModuleKind,
    sample: Sample,
    fold: &OuterFold,
    feature_id: &str,
    number: f64,
    evidence_byte: u8,
) -> ModuleOutput {
    let mut values = BTreeMap::from([(feature(feature_id), value(number))]);
    if kind == ModuleKind::Hazard {
        values.insert(
            feature("hazard_context"),
            value((sample.id() % 11) as f64 / 10.0),
        );
    }
    ModuleOutput::try_new(ModuleOutputInput {
        module_kind: kind,
        model_id: format!("{}-stacker-fixture-v1", kind.as_str()),
        model_package_hash: [kind as u8 + 0x40; 32],
        entity: btc(),
        origin: UnixNanos::new(sample.origin_time_ns()),
        as_known_at: UnixNanos::new(sample.origin_time_ns()),
        trained_through: UnixNanos::new(fold.test().start_ns() - 1),
        outer_fold_hash: fold.fold_hash(),
        values,
        availability: ModuleAvailability::Available,
        quality: QualityScore::from_millionths(920_000).expect("quality"),
        source_evidence_hash: [evidence_byte; 32],
    })
    .expect("valid module output")
}

fn schema() -> EnsembleSchema {
    EnsembleSchema::try_new(
        1,
        vec![
            MatrixColumn::new(ModuleKind::Hazard, feature("hazard_signal")),
            MatrixColumn::new(ModuleKind::Hazard, feature("hazard_context")),
            MatrixColumn::new(ModuleKind::Volatility, feature("volatility_noise")),
            MatrixColumn::new(ModuleKind::Cusp, feature("cusp_research")),
        ],
    )
    .expect("valid schema")
}

fn matrix(fold: &OuterFold, samples: &[Sample]) -> OutOfFoldMatrix {
    matrix_with_volatility_mode(fold, samples, VolatilityFixture::Mixed)
}

fn matrix_with_volatility_support(
    fold: &OuterFold,
    samples: &[Sample],
    volatility_supported: bool,
) -> OutOfFoldMatrix {
    matrix_with_volatility_mode(
        fold,
        samples,
        if volatility_supported {
            VolatilityFixture::Mixed
        } else {
            VolatilityFixture::Missing
        },
    )
}

#[derive(Clone, Copy)]
enum VolatilityFixture {
    Mixed,
    Missing,
    Constant,
    DuplicateHazard,
}

fn matrix_with_volatility_mode(
    fold: &OuterFold,
    samples: &[Sample],
    volatility_mode: VolatilityFixture,
) -> OutOfFoldMatrix {
    let rows = fold
        .test_samples(samples)
        .expect("test samples")
        .into_iter()
        .enumerate()
        .map(|(index, sample)| {
            let positive = sample.id() % 2 == 0;
            let hazard = if positive { 0.9 } else { 0.1 };
            let volatility = match volatility_mode {
                VolatilityFixture::Missing => RowModule::Missing {
                    module_kind: ModuleKind::Volatility,
                    availability: ModuleAvailability::Unavailable(
                        MissingnessReason::SourceNotSupported,
                    ),
                },
                VolatilityFixture::Constant => RowModule::output(output(
                    ModuleKind::Volatility,
                    *sample,
                    fold,
                    "volatility_noise",
                    0.5,
                    0x24,
                )),
                VolatilityFixture::DuplicateHazard => RowModule::output(output(
                    ModuleKind::Volatility,
                    *sample,
                    fold,
                    "volatility_noise",
                    hazard,
                    0x25,
                )),
                VolatilityFixture::Mixed => match index % 5 {
                    0 => RowModule::Missing {
                        module_kind: ModuleKind::Volatility,
                        availability: ModuleAvailability::Unavailable(
                            MissingnessReason::InsufficientHistory,
                        ),
                    },
                    1 | 2 => RowModule::output(output(
                        ModuleKind::Volatility,
                        *sample,
                        fold,
                        "volatility_noise",
                        0.8,
                        0x22,
                    )),
                    _ => RowModule::output(output(
                        ModuleKind::Volatility,
                        *sample,
                        fold,
                        "volatility_noise",
                        0.2,
                        0x23,
                    )),
                },
            };
            TrainingRowInput {
                sample: *sample,
                entity: btc(),
                modules: vec![
                    RowModule::output(output(
                        ModuleKind::Hazard,
                        *sample,
                        fold,
                        "hazard_signal",
                        hazard,
                        if positive { 0x31 } else { 0x32 },
                    )),
                    volatility,
                    RowModule::Missing {
                        module_kind: ModuleKind::Cusp,
                        availability: ModuleAvailability::Abstained(
                            MissingnessReason::ModelNotApplicable,
                        ),
                    },
                ],
            }
        })
        .collect();
    OutOfFoldMatrix::build(
        &schema(),
        fold,
        rows,
        CuspEligibilityReceipt::locked_without_gate(),
    )
    .expect("valid OOF matrix")
}

fn target_schema() -> StackerTargetSchema {
    StackerTargetSchema::try_from_definition(&label_definition(), DAY_SECONDS)
        .expect("valid target schema")
}

fn label_definition() -> LabelDefinition {
    label_definition_with_id("downside_stacker_fixture")
}

fn label_definition_with_id(id: &str) -> LabelDefinition {
    LabelDefinition::try_new(LabelDefinitionInput {
        id: id.to_owned(),
        version: Version::new(1, 0, 0),
        scope: DefinitionScope::Research,
        rule: LabelRule::Downside {
            threshold: PriceThreshold::AbsoluteFraction(0.05),
        },
        horizons_seconds: vec![DAY_SECONDS],
        minimum_duration_seconds: 0,
        minimum_quality_millionths: 900_000,
        minimum_source_coverage_millionths: 900_000,
        overlap_policy: OverlapPolicy::EarliestWins,
    })
    .expect("valid label definition")
}

fn target_inputs(matrix: &OutOfFoldMatrix) -> Vec<StackerTargetInput> {
    let definition_hash = label_definition().definition_hash();
    matrix
        .rows()
        .iter()
        .map(|row| StackerTargetInput {
            row_id: row.row_id(),
            origin_ns: row.origin().value(),
            as_known_at_ns: row.outcome_known_at().value(),
            horizon_seconds: DAY_SECONDS,
            definition_hash,
            source_range_hash: [u8::try_from(row.row_id() % 251 + 1).expect("bounded"); 32],
            outcome: if row.row_id() % 2 == 0 {
                LabelOutcome::Occurred { offset_seconds: 60 }
            } else {
                LabelOutcome::NotOccurred
            },
        })
        .collect()
}

fn targets(matrix: &OutOfFoldMatrix) -> StackerTargetSet {
    StackerTargetSet::try_new(matrix, target_schema(), target_inputs(matrix))
        .expect("valid targets")
}

fn config(
    constraint: WeightConstraint,
    l2_penalty: f64,
    max_iterations: u32,
    work_limit: u64,
) -> StackerConfig {
    StackerConfig::try_new(StackerConfigInput {
        l2_penalty,
        weight_constraint: constraint,
        missing_value_policy: MissingValuePolicy::TrainingMean,
        maximum_iterations: max_iterations,
        tolerance: 1.0e-7,
        initial_step: 1.0,
        minimum_step: 1.0e-12,
        maximum_backtracking_steps: 64,
        work_limit,
        additional_locked_modules: Vec::new(),
    })
    .expect("valid config")
}

fn config_with_locks(
    constraint: WeightConstraint,
    l2_penalty: f64,
    max_iterations: u32,
    work_limit: u64,
    additional_locked_modules: Vec<ModuleKind>,
) -> StackerConfig {
    StackerConfig::try_new(StackerConfigInput {
        l2_penalty,
        weight_constraint: constraint,
        missing_value_policy: MissingValuePolicy::TrainingMean,
        maximum_iterations: max_iterations,
        tolerance: 1.0e-7,
        initial_step: 1.0,
        minimum_step: 1.0e-12,
        maximum_backtracking_steps: 64,
        work_limit,
        additional_locked_modules,
    })
    .expect("valid config")
}

fn training_set<'a>(
    matrices: &'a [OutOfFoldMatrix],
    target_sets: &'a [StackerTargetSet],
) -> StackerTrainingSet {
    let folds = matrices
        .iter()
        .zip(target_sets)
        .map(|(matrix, targets)| StackerFold::new(matrix, targets))
        .collect();
    StackerTrainingSet::try_new(folds).expect("compatible OOF training folds")
}

fn column_index(model: &ensemble::MetaStacker, column_id: &str) -> usize {
    model
        .column_ids()
        .iter()
        .position(|candidate| candidate == column_id)
        .expect("model column")
}

#[test]
fn locked_nonnegative_and_simplex_weights_are_exact_and_deterministic() {
    let (folds, samples) = folds_fixture();
    let matrices = vec![matrix(&folds[0], &samples), matrix(&folds[1], &samples)];
    let target_sets = matrices.iter().map(targets).collect::<Vec<_>>();
    let training = training_set(&matrices, &target_sets);
    let simplex_config = config(WeightConstraint::Simplex, 0.01, 20_000, 50_000_000);

    let first = match ensemble::MetaStacker::fit_candidate(&training, simplex_config.clone())
        .expect("fit outcome")
    {
        StackerFitOutcome::Converged(model) => *model,
        StackerFitOutcome::NonConverged(diagnostics) => {
            panic!("simplex fit did not converge: {diagnostics:?}")
        }
    };
    let second = ensemble::MetaStacker::fit(&training, simplex_config).expect("repeated fit");

    assert_eq!(first, second);
    assert_eq!(first.column_weight("cusp_research"), Some(0.0));
    assert_eq!(first.module_weight(ModuleKind::Cusp), Some(0.0));
    let cusp_index = column_index(&first, "cusp_research");
    assert_eq!(
        first.missingness_weights()[cusp_index].to_bits(),
        0.0_f64.to_bits()
    );
    assert!(first.weights().iter().all(|weight| *weight >= 0.0));
    assert!((first.weights().iter().sum::<f64>() - 1.0).abs() <= 1.0e-12);
    assert!(first.diagnostics().converged());
    assert!(first.model_id().iter().any(|byte| *byte != 0));

    let unconstrained = ensemble::MetaStacker::fit(
        &training,
        config(WeightConstraint::Unconstrained, 0.01, 20_000, 50_000_000),
    )
    .expect("unconstrained fit still preserves mandatory locks");
    let cusp_index = column_index(&unconstrained, "cusp_research");
    assert_eq!(
        unconstrained.weights()[cusp_index].to_bits(),
        0.0_f64.to_bits()
    );
    assert_eq!(
        unconstrained.missingness_weights()[cusp_index].to_bits(),
        0.0_f64.to_bits()
    );
}

#[test]
fn constant_primary_columns_leave_simplex_geometry_and_collinearity_is_diagnosed() {
    let (folds, samples) = folds_fixture();
    let constant_matrices = vec![matrix_with_volatility_mode(
        &folds[0],
        &samples,
        VolatilityFixture::Constant,
    )];
    let constant_targets = constant_matrices.iter().map(targets).collect::<Vec<_>>();
    let constant_training = training_set(&constant_matrices, &constant_targets);
    let simplex = match ensemble::MetaStacker::fit_candidate(
        &constant_training,
        config(WeightConstraint::Simplex, 0.01, 20_000, 50_000_000),
    )
    .expect("constant-safe simplex outcome")
    {
        StackerFitOutcome::Converged(model) => *model,
        StackerFitOutcome::NonConverged(diagnostics) => {
            panic!("constant-safe simplex did not converge: {diagnostics:?}")
        }
    };
    let volatility_index = column_index(&simplex, "volatility_noise");
    assert_eq!(
        simplex.weights()[volatility_index].to_bits(),
        0.0_f64.to_bits()
    );
    assert_eq!(
        simplex.constraints().is_locked(volatility_index),
        Some(true)
    );
    assert_eq!(
        simplex
            .constraints()
            .is_missingness_locked(volatility_index),
        Some(false)
    );
    assert!((simplex.weights().iter().sum::<f64>() - 1.0).abs() <= f64::EPSILON);

    let duplicate_matrices = vec![matrix_with_volatility_mode(
        &folds[0],
        &samples,
        VolatilityFixture::DuplicateHazard,
    )];
    let duplicate_targets = duplicate_matrices.iter().map(targets).collect::<Vec<_>>();
    let duplicate_training = training_set(&duplicate_matrices, &duplicate_targets);
    let duplicate = ensemble::MetaStacker::fit(
        &duplicate_training,
        config(WeightConstraint::Unconstrained, 0.01, 20_000, 50_000_000),
    )
    .expect("regularized collinear fit");
    assert!(duplicate.diagnostics().condition_estimate() > 100.0);
}

#[test]
fn training_mean_missingness_and_inference_schema_are_explicit() {
    let (folds, samples) = folds_fixture();
    let matrices = vec![matrix(&folds[0], &samples)];
    let target_sets = matrices.iter().map(targets).collect::<Vec<_>>();
    let training = training_set(&matrices, &target_sets);
    let model = ensemble::MetaStacker::fit(
        &training,
        config(WeightConstraint::NonNegative, 0.01, 20_000, 50_000_000),
    )
    .expect("fit with reasoned missing rows");
    let row = matrices[0]
        .rows()
        .iter()
        .find(|row| {
            matches!(
                row.value("volatility_noise"),
                Some(ensemble::MatrixDatum::ModuleMissing(_))
            )
        })
        .expect("missing volatility row");
    let input = ModuleVectorInput {
        schema_hash: matrices[0].schema().evidence_hash(),
        column_ids: matrices[0]
            .columns()
            .iter()
            .map(|column| column.id().to_owned())
            .collect(),
        values: row.values().to_vec(),
        evidence_hash: [0x61; 32],
    };
    let prediction = model.predict(&input).expect("declared mean imputation");
    assert!(prediction.probability().is_finite());
    assert!((0.0..=1.0).contains(&prediction.probability()));

    let mut wrong_order = input;
    wrong_order.column_ids.swap(0, 1);
    assert_eq!(
        model.predict(&wrong_order),
        Err(EnsembleError::StackerSchemaMismatch)
    );
}

#[test]
fn missingness_indicator_distinguishes_absence_from_an_observed_training_mean() {
    let (folds, samples) = folds_fixture();
    let matrices = vec![matrix(&folds[0], &samples), matrix(&folds[1], &samples)];
    let target_sets = matrices
        .iter()
        .map(|matrix| {
            let mut inputs = target_inputs(matrix);
            for (input, row) in inputs.iter_mut().zip(matrix.rows()) {
                input.outcome = if matches!(
                    row.value("volatility_noise"),
                    Some(ensemble::MatrixDatum::ModuleMissing(_))
                ) {
                    LabelOutcome::Occurred { offset_seconds: 60 }
                } else {
                    LabelOutcome::NotOccurred
                };
            }
            StackerTargetSet::try_new(matrix, target_schema(), inputs)
                .expect("missingness-correlated targets")
        })
        .collect::<Vec<_>>();
    let training = training_set(&matrices, &target_sets);
    let model = ensemble::MetaStacker::fit(
        &training,
        config(WeightConstraint::Unconstrained, 0.01, 20_000, 50_000_000),
    )
    .expect("missingness-aware fit");
    let volatility_index = column_index(&model, "volatility_noise");
    assert!(model.missingness_weights()[volatility_index].abs() > 1.0e-3);

    let row = matrices[0]
        .rows()
        .iter()
        .find(|row| {
            matches!(
                row.value("volatility_noise"),
                Some(ensemble::MatrixDatum::ModuleMissing(_))
            )
        })
        .expect("missing volatility row");
    let missing = ModuleVectorInput {
        schema_hash: model.schema_hash(),
        column_ids: model.column_ids().to_vec(),
        values: row.values().to_vec(),
        evidence_hash: [0x73; 32],
    };
    let mut observed_mean = missing.clone();
    observed_mean.values[volatility_index] = ensemble::MatrixDatum::Present(
        FiniteF64::new(model.normalization().means()[volatility_index])
            .expect("finite training mean"),
    );
    observed_mean.evidence_hash = [0x74; 32];
    let missing_prediction = model.predict(&missing).expect("missing prediction");
    let mean_prediction = model
        .predict(&observed_mean)
        .expect("observed-mean prediction");
    assert_ne!(
        missing_prediction.logit().to_bits(),
        mean_prediction.logit().to_bits()
    );
}

#[test]
fn target_alignment_observation_and_evidence_fail_closed() {
    let (folds, samples) = folds_fixture();
    let matrix = matrix(&folds[0], &samples);
    let mut inputs = target_inputs(&matrix);
    inputs.pop();
    assert_eq!(
        StackerTargetSet::try_new(&matrix, target_schema(), inputs),
        Err(EnsembleError::TargetCoverageMismatch)
    );

    let mut inputs = target_inputs(&matrix);
    inputs[0].outcome = LabelOutcome::Censored {
        observed_seconds: 60,
    };
    assert_eq!(
        StackerTargetSet::try_new(&matrix, target_schema(), inputs),
        Err(EnsembleError::TargetNotObserved)
    );

    let mut inputs = target_inputs(&matrix);
    inputs.push(inputs[0]);
    assert_eq!(
        StackerTargetSet::try_new(&matrix, target_schema(), inputs),
        Err(EnsembleError::DuplicateTarget)
    );

    let mut inputs = target_inputs(&matrix);
    inputs[0].source_range_hash = [0; 32];
    assert_eq!(
        StackerTargetSet::try_new(&matrix, target_schema(), inputs),
        Err(EnsembleError::ZeroDigest)
    );

    let mut inputs = target_inputs(&matrix);
    inputs[0].outcome = LabelOutcome::Occurred {
        offset_seconds: DAY_SECONDS + 1,
    };
    assert_eq!(
        StackerTargetSet::try_new(&matrix, target_schema(), inputs),
        Err(EnsembleError::TargetMismatch)
    );

    let mut inputs = target_inputs(&matrix);
    inputs[0].outcome = LabelOutcome::Excluded(labels::ExclusionReason::MissingEvidence);
    assert_eq!(
        StackerTargetSet::try_new(&matrix, target_schema(), inputs),
        Err(EnsembleError::TargetNotObserved)
    );

    let mut inputs = target_inputs(&matrix);
    inputs[0].outcome = LabelOutcome::Occurred { offset_seconds: 0 };
    assert_eq!(
        StackerTargetSet::try_new(&matrix, target_schema(), inputs),
        Err(EnsembleError::TargetMismatch)
    );

    let mut inputs = target_inputs(&matrix);
    inputs[0].horizon_seconds = 3_600;
    assert_eq!(
        StackerTargetSet::try_new(&matrix, target_schema(), inputs),
        Err(EnsembleError::TargetMismatch)
    );

    assert_eq!(
        StackerTargetSchema::try_from_definition(&label_definition(), 3_600),
        Err(EnsembleError::InvalidTargetSchema)
    );

    let first_test_sample = folds[0]
        .test_samples(&samples)
        .expect("test samples")
        .first()
        .copied()
        .copied()
        .expect("first test sample");
    let mut prematurely_known_samples = samples.clone();
    let sample_index = prematurely_known_samples
        .iter()
        .position(|sample| sample.id() == first_test_sample.id())
        .expect("test sample index");
    prematurely_known_samples[sample_index] = Sample::try_new(
        first_test_sample.id(),
        first_test_sample.origin_time_ns(),
        first_test_sample.origin_time_ns(),
        first_test_sample.origin_time_ns(),
    )
    .expect("premature sample timing");
    let premature_matrix = matrix_with_volatility_mode(
        &folds[0],
        &prematurely_known_samples,
        VolatilityFixture::Mixed,
    );
    let mut premature_inputs = target_inputs(&premature_matrix);
    let premature = premature_inputs
        .iter_mut()
        .find(|input| input.row_id == first_test_sample.id())
        .expect("premature target");
    premature.outcome = LabelOutcome::Occurred { offset_seconds: 60 };
    assert_eq!(
        StackerTargetSet::try_new(&premature_matrix, target_schema(), premature_inputs),
        Err(EnsembleError::TargetMismatch)
    );

    let mut premature_inputs = target_inputs(&premature_matrix);
    let premature = premature_inputs
        .iter_mut()
        .find(|input| input.row_id == first_test_sample.id())
        .expect("premature target");
    premature.outcome = LabelOutcome::NotOccurred;
    assert_eq!(
        StackerTargetSet::try_new(&premature_matrix, target_schema(), premature_inputs),
        Err(EnsembleError::TargetMismatch)
    );
}

#[test]
fn target_and_fold_input_order_do_not_change_training_or_model_identity() {
    let (folds, samples) = folds_fixture();
    let matrices = vec![matrix(&folds[0], &samples), matrix(&folds[1], &samples)];
    let canonical_targets = matrices.iter().map(targets).collect::<Vec<_>>();
    let reversed_targets = matrices
        .iter()
        .map(|matrix| {
            let mut inputs = target_inputs(matrix);
            inputs.reverse();
            StackerTargetSet::try_new(matrix, target_schema(), inputs)
                .expect("permuted target inputs")
        })
        .collect::<Vec<_>>();
    assert_eq!(canonical_targets, reversed_targets);

    let mut offset_inputs = target_inputs(&matrices[0]);
    let occurred = offset_inputs
        .iter_mut()
        .find(|input| matches!(input.outcome, LabelOutcome::Occurred { .. }))
        .expect("occurred target");
    occurred.outcome = LabelOutcome::Occurred { offset_seconds: 61 };
    let offset_mutation = StackerTargetSet::try_new(&matrices[0], target_schema(), offset_inputs)
        .expect("valid offset mutation");
    assert_ne!(
        canonical_targets[0].evidence_hash(),
        offset_mutation.evidence_hash()
    );

    let canonical = training_set(&matrices, &canonical_targets);
    let reversed = StackerTrainingSet::try_new(vec![
        StackerFold::new(&matrices[1], &reversed_targets[1]),
        StackerFold::new(&matrices[0], &reversed_targets[0]),
    ])
    .expect("permuted fold inputs");
    assert_eq!(canonical, reversed);

    let fit_config = config(WeightConstraint::NonNegative, 0.01, 20_000, 50_000_000);
    let first = ensemble::MetaStacker::fit(&canonical, fit_config.clone()).expect("canonical fit");
    let second = ensemble::MetaStacker::fit(&reversed, fit_config).expect("permuted fit");
    assert_eq!(first, second);
}

#[test]
fn locked_module_values_cannot_affect_scores_but_remain_bound_in_prediction_lineage() {
    let (folds, samples) = folds_fixture();
    let matrices = vec![matrix(&folds[0], &samples)];
    let target_sets = matrices.iter().map(targets).collect::<Vec<_>>();
    let training = training_set(&matrices, &target_sets);
    let model = ensemble::MetaStacker::fit(
        &training,
        config(WeightConstraint::NonNegative, 0.01, 20_000, 50_000_000),
    )
    .expect("fit with locked Cusp");
    let cusp_index = column_index(&model, "cusp_research");
    assert_eq!(model.weights()[cusp_index].to_bits(), 0.0_f64.to_bits());
    assert_eq!(
        model.missingness_weights()[cusp_index].to_bits(),
        0.0_f64.to_bits()
    );

    let row = &matrices[0].rows()[0];
    let base = ModuleVectorInput {
        schema_hash: model.schema_hash(),
        column_ids: model.column_ids().to_vec(),
        values: row.values().to_vec(),
        evidence_hash: [0x71; 32],
    };
    let mut altered = base.clone();
    altered.values[cusp_index] = ensemble::MatrixDatum::Present(
        FiniteF64::new(123_456.0).expect("finite locked input mutation"),
    );
    let first = model.predict(&base).expect("base prediction");
    let second = model
        .predict(&altered)
        .expect("locked-input mutation prediction");
    assert_eq!(first.logit().to_bits(), second.logit().to_bits());
    assert_eq!(
        first.probability().to_bits(),
        second.probability().to_bits()
    );
    assert_ne!(first.evidence_hash(), second.evidence_hash());

    let mut zero_evidence = base;
    zero_evidence.evidence_hash = [0; 32];
    assert_eq!(
        model.predict(&zero_evidence),
        Err(EnsembleError::ZeroDigest)
    );
}

#[test]
fn unsupported_columns_require_an_explicit_lock_and_single_class_targets_are_rejected() {
    let (folds, samples) = folds_fixture();
    let matrices = vec![matrix_with_volatility_support(&folds[0], &samples, false)];
    let target_sets = matrices.iter().map(targets).collect::<Vec<_>>();
    let training = training_set(&matrices, &target_sets);
    assert_eq!(
        ensemble::MetaStacker::fit(
            &training,
            config(WeightConstraint::NonNegative, 0.01, 20_000, 50_000_000),
        ),
        Err(EnsembleError::UnsupportedMissingColumn)
    );
    let locked = ensemble::MetaStacker::fit(
        &training,
        config_with_locks(
            WeightConstraint::NonNegative,
            0.01,
            20_000,
            50_000_000,
            vec![ModuleKind::Volatility],
        ),
    )
    .expect("explicitly locked unsupported module");
    assert_eq!(locked.module_weight(ModuleKind::Volatility), Some(0.0));
    let volatility_index = column_index(&locked, "volatility_noise");
    assert_eq!(
        locked.missingness_weights()[volatility_index].to_bits(),
        0.0_f64.to_bits()
    );

    let matrix = matrix(&folds[1], &samples);
    let mut inputs = target_inputs(&matrix);
    for input in &mut inputs {
        input.outcome = LabelOutcome::NotOccurred;
    }
    let targets = StackerTargetSet::try_new(&matrix, target_schema(), inputs)
        .expect("observed single-class targets");
    assert_eq!(
        StackerTrainingSet::try_new(vec![StackerFold::new(&matrix, &targets)]),
        Err(EnsembleError::InsufficientTargetVariation)
    );

    assert_eq!(
        StackerConfig::try_new(StackerConfigInput {
            l2_penalty: 0.01,
            weight_constraint: WeightConstraint::NonNegative,
            missing_value_policy: MissingValuePolicy::TrainingMean,
            maximum_iterations: 100,
            tolerance: 1.0e-9,
            initial_step: 1.0,
            minimum_step: 1.0e-12,
            maximum_backtracking_steps: 64,
            work_limit: 1_000_000,
            additional_locked_modules: vec![ModuleKind::Volatility, ModuleKind::Volatility],
        }),
        Err(EnsembleError::InvalidStackerConfig)
    );
}

#[test]
fn regularization_shrinks_and_bounded_nonconvergence_returns_no_model() {
    let (folds, samples) = folds_fixture();
    let matrices = vec![matrix(&folds[0], &samples), matrix(&folds[1], &samples)];
    let target_sets = matrices.iter().map(targets).collect::<Vec<_>>();
    let training = training_set(&matrices, &target_sets);
    let weak = ensemble::MetaStacker::fit(
        &training,
        config(WeightConstraint::NonNegative, 0.001, 20_000, 50_000_000),
    )
    .expect("weak regularization");
    let strong = ensemble::MetaStacker::fit(
        &training,
        config(WeightConstraint::NonNegative, 10.0, 20_000, 50_000_000),
    )
    .expect("strong regularization");
    assert!(strong.coefficient_l2_norm() < weak.coefficient_l2_norm());

    assert_eq!(
        ensemble::MetaStacker::fit(
            &training,
            config(WeightConstraint::NonNegative, 0.01, 1, 50_000_000),
        ),
        Err(EnsembleError::StackerNonConverged)
    );
    let candidate = ensemble::MetaStacker::fit_candidate(
        &training,
        config(WeightConstraint::NonNegative, 0.01, 1, 50_000_000),
    )
    .expect("bounded candidate result");
    let StackerFitOutcome::NonConverged(diagnostics) = candidate else {
        panic!("one-iteration fit must not expose a model");
    };
    assert!(!diagnostics.converged());
    assert_eq!(diagnostics.iterations(), 1);
    assert_eq!(diagnostics.termination(), "iteration_limit");
    assert_eq!(
        ensemble::MetaStacker::fit(
            &training,
            config(WeightConstraint::NonNegative, 0.01, 20_000, 1),
        ),
        Err(EnsembleError::StackerWorkCapacity)
    );

    let invalid = StackerConfig::try_new(StackerConfigInput {
        l2_penalty: f64::NAN,
        weight_constraint: WeightConstraint::NonNegative,
        missing_value_policy: MissingValuePolicy::TrainingMean,
        maximum_iterations: 100,
        tolerance: 1.0e-9,
        initial_step: 1.0,
        minimum_step: 1.0e-12,
        maximum_backtracking_steps: 64,
        work_limit: 1_000_000,
        additional_locked_modules: Vec::new(),
    });
    assert_eq!(invalid, Err(EnsembleError::InvalidStackerConfig));
}

#[test]
fn line_search_termination_is_explicit_and_never_bypasses_projected_gradient() {
    let (folds, samples) = folds_fixture();
    let matrices = vec![matrix(&folds[0], &samples)];
    let target_sets = matrices.iter().map(targets).collect::<Vec<_>>();
    let training = training_set(&matrices, &target_sets);
    let failing_config = StackerConfig::try_new(StackerConfigInput {
        l2_penalty: 0.01,
        weight_constraint: WeightConstraint::Unconstrained,
        missing_value_policy: MissingValuePolicy::TrainingMean,
        maximum_iterations: 10,
        tolerance: 1.0e-7,
        initial_step: 1.0e6,
        minimum_step: 1.0e6,
        maximum_backtracking_steps: 1,
        work_limit: 50_000_000,
        additional_locked_modules: Vec::new(),
    })
    .expect("line-search failure config");
    let outcome = ensemble::MetaStacker::fit_candidate(&training, failing_config.clone())
        .expect("inspectable line-search outcome");
    let StackerFitOutcome::NonConverged(diagnostics) = outcome else {
        panic!("large rejected step must not expose a model");
    };
    assert_eq!(diagnostics.termination(), "line_search_failure");
    assert!(!diagnostics.converged());
    assert_eq!(
        ensemble::MetaStacker::fit(&training, failing_config),
        Err(EnsembleError::StackerNonConverged)
    );

    let weak_target_sets = matrices
        .iter()
        .map(|matrix| {
            let mut inputs = target_inputs(matrix);
            for input in &mut inputs {
                input.outcome = if input.row_id % 3 == 0 {
                    LabelOutcome::Occurred { offset_seconds: 60 }
                } else {
                    LabelOutcome::NotOccurred
                };
            }
            StackerTargetSet::try_new(matrix, target_schema(), inputs).expect("weak-signal targets")
        })
        .collect::<Vec<_>>();
    let weak_training = training_set(&matrices, &weak_target_sets);
    let floor_config = StackerConfig::try_new(StackerConfigInput {
        l2_penalty: 0.01,
        weight_constraint: WeightConstraint::Unconstrained,
        missing_value_policy: MissingValuePolicy::TrainingMean,
        maximum_iterations: 10,
        tolerance: 0.1,
        initial_step: 1.0e6,
        minimum_step: 1.0e6,
        maximum_backtracking_steps: 1,
        work_limit: 50_000_000,
        additional_locked_modules: Vec::new(),
    })
    .expect("line-search floor config");
    let outcome = ensemble::MetaStacker::fit_candidate(&weak_training, floor_config)
        .expect("inspectable line-search floor outcome");
    let StackerFitOutcome::Converged(model) = outcome else {
        panic!("declared tolerance should accept the small projected gradient");
    };
    assert_eq!(
        model.diagnostics().termination(),
        "projected_gradient_line_search_floor"
    );
    assert!(model.diagnostics().converged());
    assert!(model.diagnostics().projected_gradient_norm() <= 0.1);
}

#[test]
fn module_removal_refits_and_scores_only_on_a_distinct_oof_fold() {
    let (folds, samples) = folds_fixture();
    let training_matrices = vec![matrix(&folds[0], &samples), matrix(&folds[1], &samples)];
    let training_targets = training_matrices.iter().map(targets).collect::<Vec<_>>();
    let evaluation_matrices = vec![matrix(&folds[2], &samples)];
    let evaluation_targets = evaluation_matrices.iter().map(targets).collect::<Vec<_>>();
    let training = training_set(&training_matrices, &training_targets);
    let evaluation = training_set(&evaluation_matrices, &evaluation_targets);
    let ablation_config = config(WeightConstraint::NonNegative, 0.01, 20_000, 100_000_000);

    let report = ModuleAblationReport::run(&training, &evaluation, ablation_config.clone())
        .expect("untouched-fold ablation");
    let hazard = report.module(ModuleKind::Hazard).expect("hazard ablation");
    assert_eq!(hazard.status(), AblationStatus::Evaluated);
    assert_eq!(hazard.removed_columns().len(), 2);
    assert!(hazard.removed_columns().iter().all(|column| {
        column.primary_weight().to_bits() == 0.0_f64.to_bits()
            && column.missingness_weight().to_bits() == 0.0_f64.to_bits()
    }));
    assert!(hazard.brier_score_delta() > 0.0);
    assert!(hazard.log_loss_delta() > 0.0);
    assert_eq!(
        report
            .module(ModuleKind::Cusp)
            .expect("cusp ablation")
            .status(),
        AblationStatus::AlreadyLocked
    );
    assert!(report.evidence_hash().iter().any(|byte| *byte != 0));

    assert_eq!(
        ModuleAblationReport::run(&training, &training, ablation_config),
        Err(EnsembleError::AblationOverlap)
    );

    let future_matrices = vec![matrix(&folds[2], &samples)];
    let future_targets = future_matrices.iter().map(targets).collect::<Vec<_>>();
    let future = training_set(&future_matrices, &future_targets);
    let past_matrices = vec![matrix(&folds[1], &samples)];
    let past_targets = past_matrices.iter().map(targets).collect::<Vec<_>>();
    let past = training_set(&past_matrices, &past_targets);
    assert_eq!(
        ModuleAblationReport::run(
            &future,
            &past,
            config(WeightConstraint::NonNegative, 0.01, 20_000, 100_000_000),
        ),
        Err(EnsembleError::AblationChronology)
    );

    let alternate_definition = label_definition_with_id("downside_stacker_alternate");
    let alternate_schema =
        StackerTargetSchema::try_from_definition(&alternate_definition, DAY_SECONDS)
            .expect("alternate target schema");
    let mut alternate_inputs = target_inputs(&evaluation_matrices[0]);
    for input in &mut alternate_inputs {
        input.definition_hash = alternate_definition.definition_hash();
    }
    let alternate_targets =
        StackerTargetSet::try_new(&evaluation_matrices[0], alternate_schema, alternate_inputs)
            .expect("alternate targets");
    let incompatible = StackerTrainingSet::try_new(vec![StackerFold::new(
        &evaluation_matrices[0],
        &alternate_targets,
    )])
    .expect("alternate evaluation set");
    assert_eq!(
        ModuleAblationReport::run(
            &training,
            &incompatible,
            config(WeightConstraint::NonNegative, 0.01, 20_000, 100_000_000),
        ),
        Err(EnsembleError::AblationIncompatible)
    );
}
