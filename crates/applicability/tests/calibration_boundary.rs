use applicability::{
    ApplicabilityModel, ApplicabilityThresholds, Availability, CategoryValue, DriftDiagnostic,
    FeatureRange, NoveltyProfile, QualitySnapshot, ResidualDiagnostic, RuntimeContext,
    RuntimeContextInput, SourceEvidence, SourceHealth, SourceRequirement, TrainingObservation,
    VerifiedCalibratedInput,
};
use calibration::{
    BoundModelScore, CalibrationKey, CalibrationLineage, CalibrationPeriod, LiquidityClass,
    SelectionConfig, ValidationPartition, ValidationRow, ValidationSet, select_validation_only,
};
use dataset::{FoldBuilder, OuterFold, Sample, WalkForwardSchedule};
use hazard::{
    BucketSpec, CompetingRiskHazard, FeatureSchema, HazardConfig, HazardConfigInput, HazardOutcome,
    HazardPredictionInput, HazardSampleInput, HazardTrainingSet, HazardTrainingSetInput,
    InputQuality,
};
use labels::{
    DefinitionScope, LabelDefinition, LabelDefinitionInput, LabelEngine, LabelEvaluation,
    LabelPath, LabelRule, MarketFrameInput, OverlapPolicy, PriceThreshold,
};
use semver::Version;

const DAY: u64 = 86_400;

fn definition() -> LabelDefinition {
    LabelDefinition::try_new(LabelDefinitionInput {
        id: "applicability_downside".to_owned(),
        version: Version::new(1, 0, 0),
        scope: DefinitionScope::Research,
        rule: LabelRule::Downside {
            threshold: PriceThreshold::AbsoluteFraction(0.05),
        },
        horizons_seconds: vec![900],
        minimum_duration_seconds: 0,
        minimum_quality_millionths: 900_000,
        minimum_source_coverage_millionths: 900_000,
        overlap_policy: OverlapPolicy::EarliestWins,
    })
    .expect("definition")
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
    let schema =
        FeatureSchema::try_new(1, vec!["signal".to_owned(), "constant".to_owned()]).unwrap();
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
            let id = u64::try_from(index + 1).unwrap();
            let origin_time_ns = i64::try_from(id).unwrap() * 1_000_000_000;
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
                    + i64::try_from(observed_seconds).unwrap() * 1_000_000_000,
                features: vec![signal, 7.0],
                quality: InputQuality::Available,
                feature_lineage: [u8::try_from(index + 1).unwrap(); 32],
                label_lineage: [u8::try_from(index + 64).unwrap(); 32],
                outcome,
            }
        })
        .collect();
    let training = HazardTrainingSet::try_new(HazardTrainingSetInput {
        feature_schema: schema,
        bucket_spec: BucketSpec::v1(),
        cause_ids: vec!["downside".to_owned(), "upside".to_owned()],
        cause_definition_hashes: vec![definition().definition_hash(), [2; 32]],
        training_cutoff_ns: outer_fold().training().end_ns(),
        samples,
    })
    .unwrap();
    let config = HazardConfig::try_new(HazardConfigInput {
        lambda: 0.01,
        alpha: 0.5,
        max_iterations: 20_000,
        tolerance: 1.0e-5,
        initial_step: 1.0,
        minimum_step: 1.0e-12,
        max_backtracking: 64,
        work_limit: 500_000_000,
        minimum_at_risk_rows_per_bucket: 1,
    })
    .unwrap();
    CompetingRiskHazard::fit_for_outer_fold(&training, config, &outer_fold()).unwrap()
}

fn bound_score(model: &CompetingRiskHazard, origin_time_ns: i64, episode: &str) -> BoundModelScore {
    let signal = (origin_time_ns / 1_000_000_000 % 17) as f64 / 4.0 - 2.0;
    let prediction = model
        .predict(HazardPredictionInput {
            entity_id: "btc".to_owned(),
            episode_cluster_id: episode.to_owned(),
            feature_schema: model.feature_schema().clone(),
            origin_time_ns,
            as_known_at_ns: origin_time_ns,
            features: vec![signal, 7.0],
            quality: InputQuality::Available,
            feature_lineage: [u8::try_from(origin_time_ns.unsigned_abs() % 251 + 1).unwrap(); 32],
        })
        .unwrap();
    let forecast = prediction.at_horizon(900).unwrap();
    BoundModelScore::try_from_hazard(&forecast, &definition(), &outer_fold()).unwrap()
}

fn label(origin_time_ns: i64, episode: &str, positive: bool) -> LabelEvaluation {
    let engine = LabelEngine::try_new(LabelDefinitionInput {
        id: "applicability_downside".to_owned(),
        version: Version::new(1, 0, 0),
        scope: DefinitionScope::Research,
        rule: LabelRule::Downside {
            threshold: PriceThreshold::AbsoluteFraction(0.05),
        },
        horizons_seconds: vec![900],
        minimum_duration_seconds: 0,
        minimum_quality_millionths: 900_000,
        minimum_source_coverage_millionths: 900_000,
        overlap_policy: OverlapPolicy::EarliestWins,
    })
    .unwrap();
    let path = LabelPath::try_new_bound(
        "btc",
        origin_time_ns,
        episode,
        vec![
            MarketFrameInput::price(0, 100.0),
            MarketFrameInput::price(900, if positive { 90.0 } else { 100.0 }),
        ],
    )
    .unwrap();
    engine.evaluate_at_horizon(&path, 900).unwrap()
}

fn partition(
    model: &CompetingRiskHazard,
    key: &CalibrationKey,
    period: CalibrationPeriod,
    first_id: u64,
    namespace: &str,
) -> ValidationPartition {
    let rows = 24;
    let step = (period.end_ns - period.start_ns) / i64::from(rows + 1);
    let rows = (0..rows)
        .map(|index| {
            let origin = period.start_ns + step * i64::from(index + 1);
            let episode = format!("{namespace}_{index}");
            let score = bound_score(model, origin, &episode);
            ValidationRow::try_new(
                key,
                first_id + u64::try_from(index).unwrap(),
                &score,
                label(origin, &episode, index % 3 == 0),
                1.0,
            )
            .unwrap()
        })
        .collect();
    ValidationPartition::try_new(period, rows).unwrap()
}

#[test]
fn exact_calibrated_output_is_the_only_applicability_input() {
    let fold = outer_fold();
    let hazard = hazard_model();
    let calibration_period = fold.calibration();
    let midpoint = calibration_period.start_ns()
        + (calibration_period.end_ns() - calibration_period.start_ns()) / 2;
    let fit_period = CalibrationPeriod::new(calibration_period.start_ns(), midpoint).unwrap();
    let selector_period = CalibrationPeriod::new(midpoint, calibration_period.end_ns()).unwrap();
    let lineage_score = bound_score(&hazard, fit_period.start_ns + 1, "lineage");
    let key = CalibrationKey::try_from_score(
        &definition(),
        LiquidityClass::try_new("high").unwrap(),
        &lineage_score,
    )
    .unwrap();
    let lineage = CalibrationLineage::try_from_outer_fold(&fold, &lineage_score).unwrap();
    let validation = ValidationSet::try_new(
        key.clone(),
        lineage,
        partition(&hazard, &key, fit_period, 1, "fit"),
        partition(&hazard, &key, selector_period, 10_000, "selector"),
    )
    .unwrap();
    let artifact = select_validation_only(&validation, SelectionConfig::default()).unwrap();

    let test_origin = artifact.lineage().test_period().start_ns + 1_000_000_000;
    let score = bound_score(&hazard, test_origin, "test");
    let output = artifact.calibrate(&score).unwrap();
    let verified = VerifiedCalibratedInput::try_new(&artifact, output.clone()).unwrap();

    let novelty = NoveltyProfile::try_new(
        vec![
            FeatureRange::try_new("spread", true, -5.0, 5.0).unwrap(),
            FeatureRange::try_new("flow", false, -5.0, 5.0).unwrap(),
        ],
        vec![],
        ["kraken"],
        ["spot"],
    )
    .unwrap();
    let training: Vec<TrainingObservation> = [
        ("a", -2.0, -1.0),
        ("b", -1.0, -2.0),
        ("c", 1.0, 2.0),
        ("d", 2.0, 1.0),
    ]
    .into_iter()
    .enumerate()
    .map(|(index, (id, left, right))| {
        let observed = i64::try_from(index + 1).unwrap();
        TrainingObservation::try_new(id, observed, observed, vec![left, right], [12; 32]).unwrap()
    })
    .collect();
    let source_requirements = vec![SourceRequirement::try_new("l2", true).unwrap()];
    let model = ApplicabilityModel::fit_for_calibration(
        &artifact,
        novelty.clone(),
        source_requirements.clone(),
        training.clone(),
        ApplicabilityThresholds::default(),
        0.1,
    )
    .unwrap();
    let mut reversed_training = training.clone();
    reversed_training.reverse();
    let repeated = ApplicabilityModel::fit_for_calibration(
        &artifact,
        novelty.clone(),
        source_requirements.clone(),
        reversed_training,
        ApplicabilityThresholds::default(),
        0.1,
    )
    .unwrap();
    assert_eq!(model, repeated);
    let encoded = serde_json::to_vec(&model).unwrap();
    let decoded: ApplicabilityModel = serde_json::from_slice(&encoded).unwrap();
    assert_eq!(decoded, model);
    decoded.validate().unwrap();

    let future_training = vec![
        TrainingObservation::try_new(
            "future_a",
            artifact.lineage().training_period().end_ns + 1,
            artifact.lineage().training_period().end_ns + 1,
            vec![-2.0, -1.0],
            [21; 32],
        )
        .unwrap(),
        TrainingObservation::try_new("future_b", 2, 2, vec![-1.0, -2.0], [22; 32]).unwrap(),
        TrainingObservation::try_new("future_c", 3, 3, vec![1.0, 2.0], [23; 32]).unwrap(),
    ];
    assert!(
        ApplicabilityModel::fit_for_calibration(
            &artifact,
            novelty,
            source_requirements,
            future_training,
            ApplicabilityThresholds::default(),
            0.1,
        )
        .is_err()
    );
    let context = RuntimeContext::try_new(RuntimeContextInput {
        issued_at_ns: test_origin,
        evaluated_at_ns: test_origin + 1,
        feature_as_known_at_ns: test_origin,
        feature_schema_hash: model.feature_schema_hash(),
        input_evidence_hash: output.input_evidence_hash,
        features: vec![Some(0.0), Some(0.0)],
        categories: vec![],
        venue: "kraken".to_owned(),
        product: "spot".to_owned(),
        sources: vec![SourceEvidence::try_new("l2", SourceHealth::Healthy, [13; 32]).unwrap()],
        coefficient_drift: Some(DriftDiagnostic::try_new(0.01, test_origin, [15; 32]).unwrap()),
        residual: Some(ResidualDiagnostic::try_new(0.5, test_origin, [14; 32]).unwrap()),
        quality: QualitySnapshot::try_new(0.95, 0.95, test_origin, [16; 32]).unwrap(),
    })
    .unwrap();
    let decision = model.evaluate(&verified, &context).unwrap();
    assert_eq!(decision.availability, Availability::Experimental);
    assert_eq!(
        decision.calibrated_probability.to_bits(),
        output.calibrated_probability.to_bits()
    );
    assert_eq!(decision.horizon_seconds, output.key.horizon_seconds());

    let mut forged = output;
    forged.calibrated_probability = 0.99;
    assert!(VerifiedCalibratedInput::try_new(&artifact, forged).is_err());

    let _category_type_is_public = CategoryValue::try_new("regime", "calm").unwrap();
}
