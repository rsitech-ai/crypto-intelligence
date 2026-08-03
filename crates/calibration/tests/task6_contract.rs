use calibration::{
    BaselineForecastSet, BaselinePrediction, BoundModelScore, CalibrationError, CalibrationKey,
    CalibrationKind, CalibrationLineage, CalibrationObservation, CalibrationPeriod, CalibrationSet,
    CalibrationStatus, CalibrationUncertainty, EvaluationSet, FitConfig, HorizonScore,
    IsotonicCalibrator, LiquidityClass, PlattCalibrator, ProbabilityMetrics, RawScore,
    SelectionConfig, ValidationPartition, ValidationRow, ValidationSet,
    select_joint_validation_only, select_validation_only,
};
use dataset::{FoldBuilder, OuterFold, Sample, WalkForwardSchedule};
use ed25519_dalek::SigningKey;
use hazard::{
    BucketSpec, CompetingRiskHazard, FeatureSchema, HazardConfig, HazardConfigInput, HazardOutcome,
    HazardPredictionInput, HazardSampleInput, HazardTrainingSet, HazardTrainingSetInput,
    InputQuality,
};
use labels::{
    DefinitionScope, LabelDefinition, LabelDefinitionInput, LabelEngine, LabelEvaluation,
    LabelPath, LabelRule, MarketFrameInput, OverlapPolicy, PriceThreshold,
};
use model_registry::{
    MetricSummary, ModelPackageManifestInput, PackagePeriod, PackageSigner, QualityRequirements,
    RuntimeRequirements,
};
use semver::Version;

const DAY: u64 = 86_400;

fn definition() -> LabelDefinition {
    LabelDefinition::try_new(LabelDefinitionInput {
        id: "downside_calibration_fixture".to_owned(),
        version: Version::new(1, 0, 0),
        scope: DefinitionScope::Research,
        rule: LabelRule::Downside {
            threshold: PriceThreshold::AbsoluteFraction(0.05),
        },
        horizons_seconds: vec![900, 3_600],
        minimum_duration_seconds: 0,
        minimum_quality_millionths: 900_000,
        minimum_source_coverage_millionths: 900_000,
        overlap_policy: OverlapPolicy::EarliestWins,
    })
    .expect("valid definition")
}

fn outer_fold() -> OuterFold {
    let samples = Sample::daily_fixture(90).expect("daily samples");
    FoldBuilder::try_with_schedule(
        120,
        60,
        WalkForwardSchedule {
            initial_training_seconds: 30 * DAY,
            minimum_inner_training_seconds: 10 * DAY,
            inner_validation_seconds: 10 * DAY,
            calibration_seconds: 20 * DAY,
            test_seconds: 20 * DAY,
            step_seconds: 20 * DAY,
        },
    )
    .expect("fold builder")
    .outer_folds(&samples)
    .expect("outer folds")
    .remove(0)
}

fn hazard_model() -> CompetingRiskHazard {
    hazard_fixture(20_000, 1.0e-5).expect("hazard model")
}

fn hazard_fixture(
    max_iterations: u32,
    tolerance: f64,
) -> Result<CompetingRiskHazard, hazard::HazardError> {
    let feature_schema =
        FeatureSchema::try_new(1, vec!["signed_signal".to_owned(), "constant".to_owned()])
            .expect("schema");
    let outcomes = [
        HazardOutcome::Event {
            offset_seconds: 300,
            cause_index: 0,
        },
        HazardOutcome::Event {
            offset_seconds: 900,
            cause_index: 1,
        },
        HazardOutcome::Event {
            offset_seconds: 1_800,
            cause_index: 0,
        },
        HazardOutcome::Event {
            offset_seconds: 3_600,
            cause_index: 1,
        },
        HazardOutcome::Event {
            offset_seconds: 10_000,
            cause_index: 0,
        },
        HazardOutcome::Event {
            offset_seconds: 20_000,
            cause_index: 1,
        },
        HazardOutcome::RightCensored {
            observed_seconds: 86_400,
        },
        HazardOutcome::RightCensored {
            observed_seconds: 86_400,
        },
        HazardOutcome::RightCensored {
            observed_seconds: 86_400,
        },
        HazardOutcome::RightCensored {
            observed_seconds: 86_400,
        },
    ];
    let samples = outcomes
        .into_iter()
        .enumerate()
        .map(|(index, outcome)| {
            let id = u64::try_from(index + 1).expect("sample id");
            let origin_time_ns = i64::try_from(id).expect("time") * 1_000_000_000;
            let observed_seconds = match outcome {
                HazardOutcome::Event { offset_seconds, .. } => offset_seconds,
                HazardOutcome::RightCensored { observed_seconds } => observed_seconds,
            };
            let signal = match outcome {
                HazardOutcome::Event { cause_index: 0, .. } => 1.0 + index as f64 / 10.0,
                HazardOutcome::Event { cause_index: 1, .. } => -1.0 - index as f64 / 10.0,
                HazardOutcome::Event { .. } | HazardOutcome::RightCensored { .. } => 0.0,
            };
            HazardSampleInput {
                id,
                origin_time_ns,
                feature_as_known_at_ns: origin_time_ns,
                outcome_known_at_ns: origin_time_ns
                    + i64::try_from(observed_seconds).expect("duration") * 1_000_000_000,
                features: vec![signal, 7.0],
                quality: InputQuality::Available,
                feature_lineage: [u8::try_from(index + 1).expect("feature evidence"); 32],
                label_lineage: [u8::try_from(index + 64).expect("label evidence"); 32],
                outcome,
            }
        })
        .collect();
    let training = HazardTrainingSet::try_new(HazardTrainingSetInput {
        feature_schema,
        bucket_spec: BucketSpec::v1(),
        cause_ids: vec!["downside".to_owned(), "upside".to_owned()],
        cause_definition_hashes: vec![definition().definition_hash(), [2; 32]],
        training_cutoff_ns: outer_fold().training().end_ns(),
        samples,
    })
    .expect("hazard training");
    let config = HazardConfig::try_new(HazardConfigInput {
        lambda: 0.01,
        alpha: 0.5,
        max_iterations,
        tolerance,
        initial_step: 1.0,
        minimum_step: 1.0e-12,
        max_backtracking: 64,
        work_limit: 500_000_000,
        minimum_at_risk_rows_per_bucket: 1,
    })
    .expect("hazard config");
    CompetingRiskHazard::fit_for_outer_fold(&training, config, &outer_fold())
}

fn bound_score(model: &CompetingRiskHazard, horizon: u64, origin_time_ns: i64) -> BoundModelScore {
    let signal = (origin_time_ns / 1_000_000_000 % 17) as f64 / 4.0 - 2.0;
    bound_score_with_signal(model, horizon, origin_time_ns, signal)
}

fn bound_score_with_signal(
    model: &CompetingRiskHazard,
    horizon: u64,
    origin_time_ns: i64,
    signal: f64,
) -> BoundModelScore {
    bound_score_with_identity(
        model,
        horizon,
        origin_time_ns,
        signal,
        "asset",
        &format!("episode_{origin_time_ns}"),
        u8::try_from(origin_time_ns.unsigned_abs() % 251 + 1).expect("prediction evidence"),
    )
}

fn bound_score_with_identity(
    model: &CompetingRiskHazard,
    horizon: u64,
    origin_time_ns: i64,
    signal: f64,
    entity_id: &str,
    episode_cluster_id: &str,
    feature_evidence: u8,
) -> BoundModelScore {
    let prediction = model
        .predict(HazardPredictionInput {
            entity_id: entity_id.to_owned(),
            episode_cluster_id: episode_cluster_id.to_owned(),
            feature_schema: model.feature_schema().clone(),
            origin_time_ns,
            as_known_at_ns: origin_time_ns,
            features: vec![signal, 7.0],
            quality: InputQuality::Available,
            feature_lineage: [feature_evidence; 32],
        })
        .expect("hazard prediction");
    let forecast = prediction.at_horizon(horizon).expect("supported horizon");
    BoundModelScore::try_from_hazard(&forecast, &definition(), &outer_fold())
        .expect("bound hazard score")
}

fn label_evaluation(horizon: u64, positive: bool, origin_time_ns: i64) -> LabelEvaluation {
    label_evaluation_with_identity(
        horizon,
        positive,
        origin_time_ns,
        "asset",
        &format!("episode_{origin_time_ns}"),
    )
}

fn label_evaluation_with_identity(
    horizon: u64,
    positive: bool,
    origin_time_ns: i64,
    entity_id: &str,
    episode_cluster_id: &str,
) -> LabelEvaluation {
    let engine = LabelEngine::try_new(LabelDefinitionInput {
        id: "downside_calibration_fixture".to_owned(),
        version: Version::new(1, 0, 0),
        scope: DefinitionScope::Research,
        rule: LabelRule::Downside {
            threshold: PriceThreshold::AbsoluteFraction(0.05),
        },
        horizons_seconds: vec![900, 3_600],
        minimum_duration_seconds: 0,
        minimum_quality_millionths: 900_000,
        minimum_source_coverage_millionths: 900_000,
        overlap_policy: OverlapPolicy::EarliestWins,
    })
    .expect("label engine");
    let path = LabelPath::try_new_bound(
        entity_id,
        origin_time_ns,
        episode_cluster_id,
        (0..=horizon / 900)
            .map(|index| {
                let offset = index * 900;
                MarketFrameInput::price(
                    offset,
                    if positive && offset >= 900 {
                        90.0
                    } else {
                        100.0
                    },
                )
            })
            .collect(),
    )
    .expect("label path");
    engine
        .evaluate_at_horizon(&path, horizon)
        .expect("label evaluation")
}

struct PartitionFixture<'a> {
    first_id: u64,
    rows: usize,
    alternating_weights: [f64; 2],
    positive_every: usize,
    episode_namespace: &'a str,
}

fn partition(
    model: &CompetingRiskHazard,
    key: &CalibrationKey,
    period: CalibrationPeriod,
    fixture: PartitionFixture<'_>,
) -> ValidationPartition {
    let width = period.end_ns - period.start_ns;
    let step = width / i64::try_from(fixture.rows + 1).expect("row count");
    let values = (0..fixture.rows)
        .map(|index| {
            let index_i64 = i64::try_from(index + 1).expect("index");
            let origin = period.start_ns + step * index_i64;
            let positive = index % fixture.positive_every == 0;
            let signal = (origin / 1_000_000_000 % 17) as f64 / 4.0 - 2.0;
            let episode = format!("validation_episode_{}_{index}", fixture.episode_namespace);
            let score = bound_score_with_identity(
                model,
                key.horizon_seconds(),
                origin,
                signal,
                "asset",
                &episode,
                u8::try_from(origin.unsigned_abs() % 251 + 1).expect("prediction evidence"),
            );
            ValidationRow::try_new(
                key,
                fixture.first_id + index as u64,
                &score,
                label_evaluation_with_identity(
                    key.horizon_seconds(),
                    positive,
                    origin,
                    "asset",
                    &episode,
                ),
                fixture.alternating_weights[index % 2],
            )
            .expect("validation row")
        })
        .collect();
    ValidationPartition::try_new(period, values).expect("partition")
}

fn validation(horizon: u64, rows: usize, weight: f64) -> ValidationSet {
    validation_with_prevalence(horizon, rows, weight, 3)
}

fn validation_with_prevalence(
    horizon: u64,
    rows: usize,
    weight: f64,
    positive_every: usize,
) -> ValidationSet {
    validation_with_weights(horizon, rows, [weight, 1.0], positive_every)
}

fn validation_with_weights(
    horizon: u64,
    rows: usize,
    alternating_weights: [f64; 2],
    positive_every: usize,
) -> ValidationSet {
    let fold = outer_fold();
    let model = hazard_model();
    let outer = fold.calibration();
    let midpoint = outer.start_ns() + (outer.end_ns() - outer.start_ns()) / 2;
    let fit_period = CalibrationPeriod::new(outer.start_ns(), midpoint).expect("fit period");
    let selector_period =
        CalibrationPeriod::new(midpoint, outer.end_ns()).expect("selector period");
    let lineage_score = bound_score(&model, horizon, fit_period.start_ns + 1_000_000_000);
    let key = CalibrationKey::try_from_score(
        &definition(),
        LiquidityClass::try_new("high_liquidity").expect("class"),
        &lineage_score,
    )
    .expect("key");
    let lineage =
        CalibrationLineage::try_from_outer_fold(&fold, &lineage_score).expect("valid lineage");
    ValidationSet::try_new(
        key.clone(),
        lineage,
        partition(
            &model,
            &key,
            fit_period,
            PartitionFixture {
                first_id: 1,
                rows,
                alternating_weights,
                positive_every,
                episode_namespace: "fit",
            },
        ),
        partition(
            &model,
            &key,
            selector_period,
            PartitionFixture {
                first_id: 10_000,
                rows,
                alternating_weights,
                positive_every,
                episode_namespace: "selector",
            },
        ),
    )
    .expect("validation set")
}

#[test]
fn platt_reference_transform_is_monotone_and_extreme_logit_safe() {
    let calibrator = PlattCalibrator::new(1.2, -0.3).expect("monotone Platt");
    let values = [-1_000.0, -5.0, -1.0, 0.0, 1.0, 5.0, 1_000.0]
        .map(|value| calibrator.calibrate_logit(value).expect("finite logit"));
    assert!(values.windows(2).all(|pair| pair[0] <= pair[1]));
    assert_eq!(values[0], 0.0);
    assert_eq!(values[6], 1.0);
    assert!(PlattCalibrator::new(-0.1, 0.0).is_err());
}

#[test]
fn wilson_kish_interval_matches_the_reference_vector() {
    let interval =
        CalibrationUncertainty::try_validation_base_rate_wilson_kish(0.5, 100.0).expect("interval");
    assert!((interval.lower - 0.403_831_530_365_995_7).abs() < 1.0e-15);
    assert!((interval.upper - 0.596_168_469_634_004_4).abs() < 1.0e-15);
    assert_eq!(interval.confidence_level, 0.95);
}

fn weighted_calibration_set(name: &str, weight: f64) -> CalibrationSet {
    let outcomes = [
        false, false, true, false, true, false, true, true, false, true,
    ];
    let observations = outcomes
        .into_iter()
        .enumerate()
        .map(|(index, outcome)| {
            let origin = i64::try_from(index + 1).expect("origin") * 10;
            CalibrationObservation::from_logit(
                index as f64 - 4.5,
                outcome,
                origin,
                origin + 1,
                origin + 1,
                weight,
            )
            .expect("weighted observation")
        })
        .collect();
    CalibrationSet::new(
        name,
        CalibrationPeriod::new(1, 200).expect("period"),
        observations,
    )
    .expect("calibration set")
}

#[test]
fn importance_weight_scaling_and_maximum_weight_are_numerically_stable() {
    let light = weighted_calibration_set("weight_scale_light", 0.3);
    let heavy = weighted_calibration_set("weight_scale_heavy", 100.0);
    let light_isotonic = IsotonicCalibrator::fit(&light).expect("light isotonic");
    let heavy_isotonic = IsotonicCalibrator::fit(&heavy).expect("heavy isotonic");
    for logit in [-10.0, -2.0, 0.0, 2.0, 10.0] {
        let left = light_isotonic.calibrate_logit(logit).expect("light output");
        let right = heavy_isotonic.calibrate_logit(logit).expect("heavy output");
        assert!((left - right).abs() < 1.0e-14);
    }

    let maximum = weighted_calibration_set("maximum_valid_weight", 1.0e12);
    let platt = PlattCalibrator::fit(&maximum, FitConfig::default()).expect("maximum Platt");
    let isotonic = IsotonicCalibrator::fit(&maximum).expect("maximum isotonic");
    assert!(platt.calibrate_logit(0.0).expect("valid Platt").is_finite());
    assert!(
        isotonic
            .calibrate_logit(0.0)
            .expect("valid isotonic")
            .is_finite()
    );
    assert!(matches!(
        RawScore::try_from_logit(f64::MAX),
        Err(CalibrationError::NonFiniteArithmetic)
    ));
}

#[test]
fn uniformly_tiny_weights_survive_selection_and_artifact_round_trip() {
    let validation = validation_with_weights(900, 24, [1.0e-200; 2], 3);
    let artifact = select_validation_only(&validation, SelectionConfig::default())
        .expect("tiny uniformly scaled weights remain eligible");
    assert_eq!(artifact.support().effective_sample_size, 48.0);
    assert_eq!(artifact.support().weight_scale, 1.0e-200);
    assert_eq!(artifact.support().cluster_weight_scale, 1.0e-200);

    let encoded = serde_json::to_vec(&artifact).expect("serialize artifact");
    let decoded: calibration::CalibrationArtifact =
        serde_json::from_slice(&encoded).expect("deserialize artifact");
    assert_eq!(decoded, artifact);
    assert_eq!(
        decoded
            .registry_descriptor()
            .expect("revalidated registry descriptor")
            .evidence_blake3,
        artifact.artifact_id()
    );
}

#[test]
fn selector_is_validation_only_and_excludes_small_isotonic_fit() {
    let validation = validation(900, 24, 0.25);
    let artifact = select_validation_only(&validation, SelectionConfig::default())
        .expect("eligible validation selector");
    assert_eq!(
        artifact,
        select_validation_only(&validation, SelectionConfig::default())
            .expect("deterministic selector")
    );
    let isotonic = artifact
        .selection_report()
        .candidates
        .iter()
        .find(|candidate| candidate.kind == CalibrationKind::Isotonic)
        .expect("isotonic report");
    assert!(!isotonic.eligible);
    assert_eq!(
        isotonic.reason.as_deref(),
        Some("effective_sample_size_below_isotonic_minimum")
    );
    assert!(artifact.support().effective_sample_size < artifact.support().observations as f64);
    let descriptor = artifact
        .registry_descriptor()
        .expect("registry descriptor export");
    assert_eq!(descriptor.evidence_blake3, artifact.artifact_id());
    assert_eq!(
        descriptor.validation_period.start_ns,
        artifact.lineage().calibration_period().start_ns
    );
    assert_eq!(
        descriptor.validation_period.end_ns,
        artifact.lineage().calibration_period().end_ns
    );
    assert!(descriptor.effective_sample_size >= 8);

    let output_score = bound_score(
        &hazard_model(),
        artifact.key().horizon_seconds(),
        artifact.lineage().test_period().start_ns + 1_000_000_000,
    );
    let output_key = CalibrationKey::try_from_score(
        &definition(),
        LiquidityClass::try_new("high_liquidity").expect("class"),
        &output_score,
    )
    .expect("output key");
    assert_eq!(artifact.key().evidence_hash(), output_key.evidence_hash());
    let output = artifact
        .calibrate(&output_score)
        .expect("calibrated output");
    assert!((0.0..=1.0).contains(&output.calibrated_probability));
    assert_eq!(&output.uncertainty, artifact.base_rate_uncertainty());
    assert_eq!(
        output.uncertainty.method,
        "validation-base-rate-wilson-kish-episode-block-approximation-95-v2"
    );
    assert_eq!(output.uncertainty_scope, "validation_base_rate");
    assert_eq!(output.status, CalibrationStatus::Experimental);
    assert_ne!(output.evidence_hash, [0; 32]);
    output.verify(&artifact).expect("output verifies");
    let mut forged = output.clone();
    forged.calibrated_probability = 0.25;
    assert!(matches!(
        forged.verify(&artifact),
        Err(CalibrationError::IncompatibleArtifact)
    ));

    let research_config = SelectionConfig {
        isotonic_minimum_effective_sample_size: 8.0,
        ..SelectionConfig::default()
    };
    let research = select_validation_only(&validation, research_config).expect("research selector");
    assert!(
        research
            .selection_report()
            .candidates
            .iter()
            .any(|candidate| candidate.kind == CalibrationKind::Isotonic && candidate.eligible)
    );
}

#[test]
fn calibration_descriptor_is_accepted_by_the_signed_registry_package_boundary() {
    let validation = validation(900, 24, 1.0);
    let artifact =
        select_validation_only(&validation, SelectionConfig::default()).expect("artifact");
    let descriptor = artifact.registry_descriptor().expect("descriptor");
    let training = artifact.lineage().training_period();
    let test = artifact.lineage().test_period();
    let package = PackageSigner::sign(
        ModelPackageManifestInput {
            schema_version: 1,
            model_id: "downside_hazard_calibrated_v1".to_owned(),
            semantic_version: Version::new(1, 0, 0),
            model_family: "competing-risk-hazard".to_owned(),
            supported_assets: vec!["btc".to_owned()],
            supported_event_types: vec![artifact.key().event_type().to_owned()],
            supported_horizons_seconds: vec![artifact.key().horizon_seconds()],
            training_period: PackagePeriod::new(training.start_ns, training.end_ns)
                .expect("training period"),
            validation_period: descriptor.validation_period,
            outer_test_period: PackagePeriod::new(test.start_ns, test.end_ns).expect("test period"),
            data_hash: [1; 32],
            feature_schema_hash: [2; 32],
            normalization_hash: [3; 32],
            label_definition_hash: artifact.key().label_definition_hash(),
            code_commit: "0123456789abcdef0123456789abcdef01234567".to_owned(),
            hyperparameters_blake3: [5; 32],
            calibration: descriptor,
            quality_requirements: QualityRequirements {
                minimum_quality_millionths: 900_000,
                minimum_source_count: 2,
                minimum_outer_test_positives: 2,
                minimum_shadow_positives: 2,
                minimum_brier_skill_score: 0.0,
                maximum_log_loss: 1.0,
                calibration_slope_minimum: 0.5,
                calibration_slope_maximum: 1.5,
                calibration_intercept_absolute_maximum: 0.5,
            },
            runtime_requirements: RuntimeRequirements {
                minimum_runtime_version: Version::new(1, 0, 0),
                required_capabilities: vec!["calibration-v1".to_owned()],
                minimum_memory_bytes: 1_024,
            },
            metrics: MetricSummary {
                outer_test_evidence_blake3: [6; 32],
                observations: 16,
                positives: 4,
                log_loss: 0.5,
                brier_skill_score: 0.1,
                calibration_intercept: 0.0,
                calibration_slope: 1.0,
            },
            subgroup_metrics_blake3: [7; 32],
            limitations: vec!["fixture package is not production evidence".to_owned()],
            model_card_blake3: [8; 32],
            review_after_ns: test.end_ns.checked_add(1).expect("review time"),
            expires_at_ns: test.end_ns.checked_add(2).expect("expiry time"),
        },
        b"calibration-registry-integration-fixture",
        "calibration_fixture_key",
        &SigningKey::from_bytes(&[0x42; 32]),
    )
    .expect("signed model package");
    assert_eq!(
        package.manifest.calibration.evidence_blake3,
        artifact.artifact_id()
    );
}

#[test]
fn bounded_optimizer_returns_no_artifact_without_convergence() {
    let validation = validation(900, 24, 1.0);
    let config = SelectionConfig {
        fit: calibration::FitConfig {
            maximum_iterations: 1,
            ..calibration::FitConfig::default()
        },
        ..SelectionConfig::default()
    };
    assert!(matches!(
        select_validation_only(&validation, config),
        Err(CalibrationError::NoEligibleCandidate)
    ));
}

#[test]
fn one_candidate_failure_is_recorded_without_aborting_an_eligible_fallback() {
    let validation = validation(900, 24, 1.0);
    let artifact = select_validation_only(
        &validation,
        SelectionConfig {
            fit: calibration::FitConfig {
                maximum_iterations: 1,
                ..calibration::FitConfig::default()
            },
            isotonic_minimum_effective_sample_size: 8.0,
            ..SelectionConfig::default()
        },
    )
    .expect("isotonic fallback");
    assert_eq!(
        artifact.selection_report().selected,
        CalibrationKind::Isotonic
    );
    assert!(
        artifact
            .selection_report()
            .candidates
            .iter()
            .any(|candidate| {
                candidate.kind == CalibrationKind::Platt
                    && !candidate.eligible
                    && candidate.reason.as_deref() == Some("optimizer_nonconvergence")
            })
    );
}

#[test]
fn outcome_knowledge_and_censoring_fail_closed() {
    let validation = validation(900, 24, 1.0);
    let key = validation.key();
    let origin = validation.lineage().calibration_period().start_ns + 1_000_000_000;
    let score = bound_score(&hazard_model(), 900, origin);
    let engine = LabelEngine::try_new(LabelDefinitionInput {
        id: "downside_calibration_fixture".to_owned(),
        version: Version::new(1, 0, 0),
        scope: DefinitionScope::Research,
        rule: LabelRule::Downside {
            threshold: PriceThreshold::AbsoluteFraction(0.05),
        },
        horizons_seconds: vec![900, 3_600],
        minimum_duration_seconds: 0,
        minimum_quality_millionths: 900_000,
        minimum_source_coverage_millionths: 900_000,
        overlap_policy: OverlapPolicy::EarliestWins,
    })
    .expect("engine");
    let censored_path = LabelPath::try_new_bound(
        "asset",
        origin,
        format!("episode_{origin}"),
        vec![
            MarketFrameInput::price(0, 100.0),
            MarketFrameInput::price(300, 100.0),
        ],
    )
    .expect("censored path");
    let censored = engine
        .evaluate_at_horizon(&censored_path, 900)
        .expect("censored evaluation");
    assert!(matches!(
        ValidationRow::try_new(key, 1, &score, censored, 1.0),
        Err(CalibrationError::IneligibleOutcome)
    ));
}

#[test]
fn label_entity_origin_and_episode_must_match_the_typed_score() {
    let validation = validation(900, 24, 1.0);
    let key = validation.key();
    let origin = validation.lineage().calibration_period().start_ns + 1_000_000_000;
    let score = bound_score(&hazard_model(), 900, origin);
    let wrong_entity = label_evaluation_with_identity(
        900,
        true,
        origin,
        "other_asset",
        &format!("episode_{origin}"),
    );
    let wrong_origin = label_evaluation_with_identity(
        900,
        true,
        origin + 1_000_000_000,
        "asset",
        &format!("episode_{origin}"),
    );
    let wrong_episode = label_evaluation_with_identity(900, true, origin, "asset", "other_episode");
    for label in [wrong_entity, wrong_origin, wrong_episode] {
        assert!(matches!(
            ValidationRow::try_new(key, 1, &score, label, 1.0),
            Err(CalibrationError::InvalidRow)
        ));
    }
}

#[test]
fn hazard_score_rejects_a_different_label_definition_for_the_same_cause() {
    let fold = outer_fold();
    let model = hazard_model();
    let origin = fold.calibration().start_ns() + 1_000_000_000;
    let prediction = model
        .predict(HazardPredictionInput {
            entity_id: "asset".to_owned(),
            episode_cluster_id: "definition_probe".to_owned(),
            feature_schema: model.feature_schema().clone(),
            origin_time_ns: origin,
            as_known_at_ns: origin,
            features: vec![0.25, 7.0],
            quality: InputQuality::Available,
            feature_lineage: [19; 32],
        })
        .expect("prediction");
    let forecast = prediction.at_horizon(900).expect("forecast");
    let different_definition = LabelDefinition::try_new(LabelDefinitionInput {
        id: "downside_calibration_fixture".to_owned(),
        version: Version::new(1, 0, 1),
        scope: DefinitionScope::Research,
        rule: LabelRule::Downside {
            threshold: PriceThreshold::AbsoluteFraction(0.06),
        },
        horizons_seconds: vec![900, 3_600],
        minimum_duration_seconds: 0,
        minimum_quality_millionths: 900_000,
        minimum_source_coverage_millionths: 900_000,
        overlap_policy: OverlapPolicy::EarliestWins,
    })
    .expect("different definition");
    assert!(matches!(
        BoundModelScore::try_from_hazard(&forecast, &different_definition, &fold),
        Err(CalibrationError::IncompatibleArtifact)
    ));
}

#[test]
fn nonconverged_hazard_cannot_be_fold_bound_for_calibration() {
    assert!(matches!(
        hazard_fixture(1, 1.0e-12),
        Err(hazard::HazardError::NonConvergedCandidate)
    ));
}

#[test]
fn fitted_model_cannot_be_restamped_for_another_outer_fold() {
    let model_fold = outer_fold();
    let other_fold = FoldBuilder::try_with_schedule(
        120,
        60,
        WalkForwardSchedule {
            initial_training_seconds: 30 * DAY,
            minimum_inner_training_seconds: 10 * DAY,
            inner_validation_seconds: 10 * DAY,
            calibration_seconds: 20 * DAY,
            test_seconds: 20 * DAY,
            step_seconds: 20 * DAY,
        },
    )
    .expect("fold builder")
    .outer_folds(&Sample::daily_fixture(110).expect("samples"))
    .expect("folds")
    .remove(1);
    assert_ne!(model_fold.fold_hash(), other_fold.fold_hash());
    let model = hazard_model();
    let origin = model_fold.calibration().start_ns() + 1_000_000_000;
    let forecast = model
        .predict(HazardPredictionInput {
            entity_id: "asset".to_owned(),
            episode_cluster_id: "fold_probe".to_owned(),
            feature_schema: model.feature_schema().clone(),
            origin_time_ns: origin,
            as_known_at_ns: origin,
            features: vec![0.25, 7.0],
            quality: InputQuality::Available,
            feature_lineage: [23; 32],
        })
        .expect("prediction")
        .at_horizon(900)
        .expect("forecast");
    assert!(matches!(
        BoundModelScore::try_from_hazard(&forecast, &definition(), &other_fold),
        Err(CalibrationError::InvalidLineage)
    ));
}

#[test]
fn clustered_support_uses_aggregated_episode_weights() {
    let fold = outer_fold();
    let model = hazard_model();
    let outer = fold.calibration();
    let midpoint = outer.start_ns() + (outer.end_ns() - outer.start_ns()) / 2;
    let fit_period = CalibrationPeriod::new(outer.start_ns(), midpoint).expect("fit period");
    let selector_period =
        CalibrationPeriod::new(midpoint, outer.end_ns()).expect("selector period");
    let lineage_score = bound_score(&model, 900, fit_period.start_ns + 1_000_000_000);
    let key = CalibrationKey::try_from_score(
        &definition(),
        LiquidityClass::try_new("high_liquidity").expect("class"),
        &lineage_score,
    )
    .expect("key");
    let lineage = CalibrationLineage::try_from_outer_fold(&fold, &lineage_score).expect("lineage");
    let make_partition = |period: CalibrationPeriod, first_id: u64| {
        let width = period.end_ns - period.start_ns;
        let rows = (0..8)
            .map(|index| {
                let origin = period.start_ns + width * i64::from(index + 1) / 9;
                let episode = if index == 7 {
                    format!("minority_episode_{first_id}")
                } else {
                    format!("dominant_episode_{first_id}")
                };
                let score = bound_score_with_identity(
                    &model,
                    900,
                    origin,
                    index as f64 / 4.0 - 1.0,
                    "asset",
                    &episode,
                    u8::try_from(index + 1).expect("evidence"),
                );
                ValidationRow::try_new(
                    &key,
                    first_id + index as u64,
                    &score,
                    label_evaluation_with_identity(900, index % 2 == 0, origin, "asset", &episode),
                    if index == 7 { 1.0 } else { 10.0 },
                )
                .expect("row")
            })
            .collect();
        ValidationPartition::try_new(period, rows).expect("partition")
    };
    let fit = make_partition(fit_period, 1);
    let selector = make_partition(selector_period, 100);
    let validation = ValidationSet::try_new(key, lineage, fit, selector).expect("validation");
    let config = SelectionConfig {
        minimum_effective_sample_size: 2.0,
        isotonic_minimum_effective_sample_size: 2.0,
        ..SelectionConfig::default()
    };
    assert!(matches!(
        select_validation_only(&validation, config),
        Err(CalibrationError::InsufficientSupport)
    ));
}

#[test]
fn one_episode_block_cannot_cross_fit_and_selector_partitions() {
    let fold = outer_fold();
    let model = hazard_model();
    let outer = fold.calibration();
    let midpoint = outer.start_ns() + (outer.end_ns() - outer.start_ns()) / 2;
    let fit_period = CalibrationPeriod::new(outer.start_ns(), midpoint).expect("fit period");
    let selector_period =
        CalibrationPeriod::new(midpoint, outer.end_ns()).expect("selector period");
    let score = bound_score(&model, 900, fit_period.start_ns + 1_000_000_000);
    let key = CalibrationKey::try_from_score(
        &definition(),
        LiquidityClass::try_new("high_liquidity").expect("class"),
        &score,
    )
    .expect("key");
    let lineage = CalibrationLineage::try_from_outer_fold(&fold, &score).expect("lineage");
    let fit = partition(
        &model,
        &key,
        fit_period,
        PartitionFixture {
            first_id: 1,
            rows: 8,
            alternating_weights: [1.0; 2],
            positive_every: 2,
            episode_namespace: "shared",
        },
    );
    let selector = partition(
        &model,
        &key,
        selector_period,
        PartitionFixture {
            first_id: 100,
            rows: 8,
            alternating_weights: [1.0; 2],
            positive_every: 2,
            episode_namespace: "shared",
        },
    );
    assert!(matches!(
        ValidationSet::try_new(key, lineage, fit, selector),
        Err(CalibrationError::ValidationLeakage)
    ));
}

#[test]
fn equal_maximum_weight_episode_blocks_clamp_kish_to_cluster_count() {
    let fold = outer_fold();
    let model = hazard_model();
    let outer = fold.calibration();
    let midpoint = outer.start_ns() + (outer.end_ns() - outer.start_ns()) / 2;
    let fit_period = CalibrationPeriod::new(outer.start_ns(), midpoint).expect("fit period");
    let selector_period =
        CalibrationPeriod::new(midpoint, outer.end_ns()).expect("selector period");
    let lineage_score = bound_score(&model, 900, fit_period.start_ns + 1_000_000_000);
    let key = CalibrationKey::try_from_score(
        &definition(),
        LiquidityClass::try_new("high_liquidity").expect("class"),
        &lineage_score,
    )
    .expect("key");
    let lineage = CalibrationLineage::try_from_outer_fold(&fold, &lineage_score).expect("lineage");
    let make_partition = |period: CalibrationPeriod, first_id: u64, namespace: &str| {
        let width = period.end_ns - period.start_ns;
        let rows = (0..10)
            .map(|index| {
                let origin = period.start_ns + width * i64::from(index + 1) / 11;
                let episode = format!("max_weight_{namespace}_{}", index % 5);
                let score = bound_score_with_identity(
                    &model,
                    900,
                    origin,
                    index as f64 / 3.0 - 1.0,
                    "asset",
                    &episode,
                    u8::try_from(index + 1).expect("evidence"),
                );
                ValidationRow::try_new(
                    &key,
                    first_id + index as u64,
                    &score,
                    label_evaluation_with_identity(900, index % 2 == 0, origin, "asset", &episode),
                    1.0e12,
                )
                .expect("row")
            })
            .collect();
        ValidationPartition::try_new(period, rows).expect("partition")
    };
    let fit = make_partition(fit_period, 1, "fit");
    let selector = make_partition(selector_period, 100, "selector");
    let validation = ValidationSet::try_new(key, lineage, fit, selector).expect("validation");
    let artifact = select_validation_only(
        &validation,
        SelectionConfig {
            minimum_effective_sample_size: 5.0,
            isotonic_minimum_effective_sample_size: 5.0,
            ..SelectionConfig::default()
        },
    )
    .expect("artifact");
    assert_eq!(artifact.support().independent_clusters, 10);
    assert_eq!(artifact.support().effective_sample_size, 10.0);
}

#[test]
fn serialized_lineage_and_artifact_mutations_are_rejected() {
    let validation = validation(900, 24, 1.0);
    let artifact =
        select_validation_only(&validation, SelectionConfig::default()).expect("artifact");
    let mut forged = serde_json::to_value(&artifact).expect("artifact JSON");
    forged["key"]["horizon_seconds"] = serde_json::json!(3_600);
    let forged = serde_json::from_value(forged).expect("structural artifact");
    let score = bound_score(
        &hazard_model(),
        900,
        artifact.lineage().test_period().start_ns + 1_000_000_000,
    );
    assert!(matches!(
        calibration::CalibrationArtifact::calibrate(&forged, &score),
        Err(CalibrationError::InvalidLineage) | Err(CalibrationError::InvalidCalibrator)
    ));

    let mut forged_metric = serde_json::to_value(&artifact).expect("artifact JSON");
    forged_metric["selection"]["config"]["metric"] = serde_json::json!("brier");
    let forged_metric = serde_json::from_value(forged_metric).expect("structural artifact");
    assert!(matches!(
        calibration::CalibrationArtifact::calibrate(&forged_metric, &score),
        Err(CalibrationError::InvalidCalibrator)
    ));

    let mut forged_counts = serde_json::to_value(&artifact).expect("artifact JSON");
    forged_counts["support"]["observations"] = serde_json::json!(u64::MAX);
    forged_counts["support"]["positives"] = serde_json::json!(u64::MAX);
    forged_counts["support"]["negatives"] = serde_json::json!(u64::MAX);
    let forged_counts: calibration::CalibrationArtifact =
        serde_json::from_value(forged_counts).expect("structural artifact");
    assert!(matches!(
        forged_counts.calibrate(&score),
        Err(CalibrationError::InvalidObservationWeight)
    ));
    assert!(matches!(
        forged_counts.registry_descriptor(),
        Err(CalibrationError::InvalidObservationWeight)
    ));

    let mut forged_partition_counts = serde_json::to_value(&artifact).expect("artifact JSON");
    for partition in ["fit_support", "selector_support"] {
        forged_partition_counts["selection"][partition]["observations"] =
            serde_json::json!(u64::MAX);
        forged_partition_counts["selection"][partition]["positives"] = serde_json::json!(0);
        forged_partition_counts["selection"][partition]["negatives"] = serde_json::json!(u64::MAX);
        forged_partition_counts["selection"][partition]["positive_weight"] = serde_json::json!(0.0);
    }
    let forged_partition_counts: calibration::CalibrationArtifact =
        serde_json::from_value(forged_partition_counts).expect("structural artifact");
    assert!(matches!(
        forged_partition_counts.calibrate(&score),
        Err(CalibrationError::InvalidCalibrator)
    ));
    assert!(matches!(
        forged_partition_counts.registry_descriptor(),
        Err(CalibrationError::InvalidCalibrator)
    ));
}

#[test]
fn untouched_evaluation_is_bound_to_the_exact_frozen_artifact() {
    let validation = validation(900, 24, 1.0);
    let artifact =
        select_validation_only(&validation, SelectionConfig::default()).expect("artifact");
    let period = artifact.lineage().test_period();
    let width = period.end_ns - period.start_ns;
    let model = hazard_model();
    let observations: Vec<ValidationRow> = (0..16)
        .map(|index| {
            let origin = period.start_ns + width * i64::from(index + 1) / 17;
            let positive = index % 4 == 0;
            let score = bound_score(&model, artifact.key().horizon_seconds(), origin);
            ValidationRow::try_new(
                artifact.key(),
                u64::try_from(index + 1).expect("row id"),
                &score,
                label_evaluation(artifact.key().horizon_seconds(), positive, origin),
                if index == 0 { 10.0 } else { 1.0 },
            )
            .expect("outer-test observation")
        })
        .collect();
    let wrong_population = EvaluationSet::new(
        &artifact,
        "outer_test_other_population",
        observations.clone(),
    )
    .expect("other bound evaluation");
    let evaluation =
        EvaluationSet::new(&artifact, "outer_test_fold", observations).expect("bound evaluation");
    let baseline = BaselineForecastSet::try_new(
        &evaluation,
        "strongest_baseline",
        (0..16)
            .map(|index| BaselinePrediction {
                row_id: u64::try_from(index + 1).expect("row id"),
                entity_id: "asset".to_owned(),
                probability: 0.25,
                source_evidence_hash: [u8::try_from(index + 1).expect("evidence"); 32],
            })
            .collect(),
    )
    .expect("baseline evidence");
    let metrics =
        ProbabilityMetrics::evaluate(&evaluation, &artifact, 4, std::slice::from_ref(&baseline))
            .expect("metrics");
    assert_eq!(metrics.observations, 16);
    assert!((metrics.base_rate - 0.52).abs() < 1.0e-12);
    assert_eq!(metrics.total_weight, 25.0);
    assert!(metrics.effective_sample_size < metrics.observations as f64);
    assert_eq!(metrics.evaluation_evidence_hash, evaluation.evidence_hash());
    assert_eq!(
        metrics.calibration_method.as_deref(),
        Some(artifact.method_name())
    );
    assert!(matches!(
        ProbabilityMetrics::evaluate(
            &wrong_population,
            &artifact,
            4,
            std::slice::from_ref(&baseline)
        ),
        Err(CalibrationError::UndefinedMetric)
    ));

    let mut forged = serde_json::to_value(evaluation).expect("evaluation JSON");
    forged["artifact_id"] = serde_json::to_value([8_u8; 32]).expect("artifact id JSON");
    let forged: EvaluationSet = serde_json::from_value(forged).expect("structural evaluation");
    assert!(matches!(
        ProbabilityMetrics::evaluate(&forged, &artifact, 4, &[baseline]),
        Err(CalibrationError::IncompatibleArtifact) | Err(CalibrationError::InvalidLineage)
    ));
}

#[test]
fn adaptive_calibration_error_is_invariant_to_tied_score_order() {
    let validation = validation(900, 24, 1.0);
    let artifact =
        select_validation_only(&validation, SelectionConfig::default()).expect("artifact");
    let period = artifact.lineage().test_period();
    let model = hazard_model();
    let make = |interleaved: bool| {
        (0..16)
            .map(|index| {
                let group_index = index % 8;
                let positive = if interleaved {
                    group_index == 1 || group_index == 6
                } else {
                    group_index < 2
                };
                let origin =
                    period.start_ns + (period.end_ns - period.start_ns) * i64::from(index + 1) / 17;
                let score = bound_score_with_signal(
                    &model,
                    artifact.key().horizon_seconds(),
                    origin,
                    if index < 8 { -0.5 } else { 0.5 },
                );
                ValidationRow::try_new(
                    artifact.key(),
                    u64::try_from(index + 1).expect("row id"),
                    &score,
                    label_evaluation(artifact.key().horizon_seconds(), positive, origin),
                    1.0,
                )
                .expect("tied observation")
            })
            .collect()
    };
    let first = EvaluationSet::new(&artifact, "tied_order_a", make(false)).expect("first");
    let second = EvaluationSet::new(&artifact, "tied_order_b", make(true)).expect("second");
    let baseline_predictions = || {
        (0..16)
            .map(|index| BaselinePrediction {
                row_id: u64::try_from(index + 1).expect("row id"),
                entity_id: "asset".to_owned(),
                probability: 0.25,
                source_evidence_hash: [u8::try_from(index + 1).expect("evidence"); 32],
            })
            .collect()
    };
    let first_baseline =
        BaselineForecastSet::try_new(&first, "strongest_baseline", baseline_predictions())
            .expect("first baseline");
    let second_baseline =
        BaselineForecastSet::try_new(&second, "strongest_baseline", baseline_predictions())
            .expect("second baseline");
    let first = ProbabilityMetrics::evaluate(&first, &artifact, 4, &[first_baseline])
        .expect("first metrics");
    let second = ProbabilityMetrics::evaluate(&second, &artifact, 4, &[second_baseline])
        .expect("second metrics");
    assert_eq!(
        first.adaptive_calibration_error,
        second.adaptive_calibration_error
    );
}

#[test]
fn constant_calibrated_logits_report_regression_as_rank_deficient() {
    let validation = validation(900, 24, 1.0);
    let artifact =
        select_validation_only(&validation, SelectionConfig::default()).expect("artifact");
    let period = artifact.lineage().test_period();
    let model = hazard_model();
    let rows = (0..16)
        .map(|index| {
            let origin =
                period.start_ns + (period.end_ns - period.start_ns) * i64::from(index + 1) / 17;
            let score =
                bound_score_with_signal(&model, artifact.key().horizon_seconds(), origin, 0.25);
            ValidationRow::try_new(
                artifact.key(),
                u64::try_from(index + 1).expect("row id"),
                &score,
                label_evaluation(artifact.key().horizon_seconds(), index % 4 == 0, origin),
                1.0,
            )
            .expect("row")
        })
        .collect();
    let evaluation = EvaluationSet::new(&artifact, "constant_logits", rows).expect("evaluation");
    let baseline = BaselineForecastSet::try_new(
        &evaluation,
        "constant_baseline",
        (0..16)
            .map(|index| BaselinePrediction {
                row_id: u64::try_from(index + 1).expect("row id"),
                entity_id: "asset".to_owned(),
                probability: 0.25,
                source_evidence_hash: [u8::try_from(index + 1).expect("evidence"); 32],
            })
            .collect(),
    )
    .expect("baseline");
    let metrics = ProbabilityMetrics::evaluate(&evaluation, &artifact, 4, &[baseline])
        .expect("metrics remain available");
    assert_eq!(metrics.calibration_slope, None);
    assert_eq!(metrics.calibration_intercept, None);
    assert_eq!(
        metrics.calibration_regression_status,
        "not_estimable_rank_deficient"
    );
}

#[test]
fn shared_joint_transform_preserves_cumulative_horizon_order() {
    let short = validation(900, 24, 1.0);
    let long = validation_with_prevalence(3_600, 24, 1.0, 2);
    assert!(matches!(
        select_joint_validation_only(&[long.clone(), short.clone()], SelectionConfig::default()),
        Err(CalibrationError::IncompatibleArtifact)
    ));
    let joint = select_joint_validation_only(&[short, long], SelectionConfig::default())
        .expect("joint calibration");
    assert_ne!(
        joint.artifacts()[0].validation_base_rate(),
        joint.artifacts()[1].validation_base_rate()
    );
    assert!(
        joint
            .artifacts()
            .iter()
            .all(|artifact| artifact.support().observations == 48)
    );
    assert!(
        joint.artifacts()[0]
            .selection_report()
            .fit_support
            .effective_sample_size
            <= 24.0
    );
    let model = hazard_model();
    let origin = outer_fold().test().start_ns() + 1_000_000_000;
    let short_score = bound_score(&model, 900, origin);
    let long_score = bound_score(&model, 3_600, origin);
    for (artifact, score) in joint.artifacts().iter().zip([&short_score, &long_score]) {
        let score_key = CalibrationKey::try_from_score(
            &definition(),
            LiquidityClass::try_new("high_liquidity").expect("class"),
            score,
        )
        .expect("score key");
        assert_eq!(artifact.key().raw_model_hash(), score_key.raw_model_hash());
        assert_eq!(
            artifact.key().raw_model_training_hash(),
            score_key.raw_model_training_hash()
        );
        assert_eq!(
            artifact.key().horizon_seconds(),
            score_key.horizon_seconds()
        );
        assert_eq!(artifact.key().evidence_hash(), score_key.evidence_hash());
    }
    let outputs = joint
        .calibrate_curve(&[
            HorizonScore::try_new(short_score.clone()).expect("short horizon score"),
            HorizonScore::try_new(long_score.clone()).expect("long horizon score"),
        ])
        .expect("coherent curve");
    assert!(outputs[0].calibrated_probability <= outputs[1].calibrated_probability);

    let original_signal = (origin / 1_000_000_000 % 17) as f64 / 4.0 - 2.0;
    let mismatched_input = bound_score_with_identity(
        &model,
        3_600,
        origin,
        original_signal + 0.5,
        "asset",
        &format!("episode_{origin}"),
        u8::try_from(origin.unsigned_abs() % 251 + 1).expect("evidence"),
    );
    assert!(matches!(
        joint.calibrate_curve(&[
            HorizonScore::try_new(short_score.clone()).expect("short horizon score"),
            HorizonScore::try_new(mismatched_input).expect("altered input score"),
        ]),
        Err(CalibrationError::HorizonIncoherence)
    ));

    assert!(matches!(
        joint.calibrate_curve(&[
            HorizonScore::try_new(long_score).expect("long horizon score"),
            HorizonScore::try_new(short_score).expect("short horizon score"),
        ]),
        Err(CalibrationError::HorizonIncoherence)
    ));
}
