use std::collections::BTreeMap;

use cusp::{
    CoefficientSign, ControlCovariance, ControlDatum, ControlError, ControlFeature,
    ControlFeatureKey, ControlMap, ControlMissingReason, ControlSchema, ControlTarget,
    ControlValue, ControlVector, FeatureCoefficient, FeatureCovariance, FeatureGroup,
    LinearControl,
};
use domain::{AssetId, AssetNamespace};
use feature_registry::{
    DurationNanos, EntityScope, EventTimePolicy, FeatureConsumptionRole, FeatureDefinition,
    FeatureDefinitionInput, FeatureDocumentation, FeatureId, FeatureRegistry, FeatureStatus,
    FeatureValueType, FormulaHash, InputRequirement, MissingnessPolicy, NormalizationKind,
    NormalizationPolicy, QualityRequirement, QualityScore, WindowDefinition, WindowId, WindowKind,
};
use quality::SourceHealthState;
use semver::Version;

#[test]
fn shared_and_asset_specific_terms_sum_exactly() {
    let (map, btc, _) = fixture();
    let evaluation = map
        .evaluate_detailed(&btc, &complete_vector(), None)
        .expect("valid control evaluation");

    assert_relative(evaluation.controls.alpha, 2.0, 1e-14);
    assert_relative(evaluation.controls.beta, 1.0, 1e-14);
    assert!(evaluation.missing_optional.is_empty());
    assert_eq!(evaluation.sensitivities.len(), 2);
    assert_relative(evaluation.sensitivities[0].alpha, 1.25, 1e-14);
    assert_relative(evaluation.sensitivities[0].beta, 0.5, 1e-14);
    assert_relative(evaluation.sensitivities[1].alpha, 1.0, 1e-14);
    assert_relative(evaluation.sensitivities[1].beta, 0.0, 1e-14);
}

#[test]
fn asset_generation_is_part_of_the_override_identity() {
    let (map, btc_generation_one, btc_generation_two) = fixture();
    let generation_one = map
        .evaluate(&btc_generation_one, &complete_vector())
        .expect("generation one");
    let generation_two = map
        .evaluate(&btc_generation_two, &complete_vector())
        .expect("generation two");

    assert_relative(generation_one.alpha, 2.0, 1e-14);
    assert_relative(generation_two.alpha, 1.75, 1e-14);
    assert_relative(generation_one.beta, generation_two.beta, 1e-14);
}

#[test]
fn missing_required_abstains_and_optional_missingness_is_explicit() {
    let (map, btc, _) = fixture();
    let required_missing = ControlVector::try_new(vec![
        ControlValue::missing(key("signal_a"), ControlMissingReason::Stale),
        ControlValue::present(key("signal_b"), 0.5).expect("finite feature"),
    ])
    .expect("bounded vector");
    assert!(matches!(
        map.evaluate(&btc, &required_missing),
        Err(ControlError::RequiredFeatureMissing { .. })
    ));

    let optional_missing = ControlVector::try_new(vec![
        ControlValue::present(key("signal_a"), 2.0).expect("finite feature"),
        ControlValue::missing(key("signal_b"), ControlMissingReason::InsufficientHistory),
    ])
    .expect("bounded vector");
    let evaluation = map
        .evaluate_detailed(&btc, &optional_missing, None)
        .expect("optional missing feature");
    assert_relative(evaluation.controls.alpha, 1.5, 1e-14);
    assert_eq!(evaluation.missing_optional.len(), 1);
    assert_eq!(evaluation.missing_optional[0].key, key("signal_b"));
    assert_eq!(
        evaluation.missing_optional[0].reason,
        ControlMissingReason::InsufficientHistory
    );
}

#[test]
fn constraints_overlap_and_sparse_penalties_are_machine_enforced() {
    let (map, _, _) = fixture();
    assert_relative(map.sparse_penalty(), 0.8, 1e-14);

    let mut invalid_alpha = map.clone().into_input();
    invalid_alpha.alpha.shared[0].coefficient = -1.0;
    assert!(matches!(
        ControlMap::try_new(invalid_alpha),
        Err(ControlError::SignConstraintViolation { .. })
    ));

    let mut undeclared_overlap = map.into_input();
    undeclared_overlap.schema.features[0].target = ControlTarget::Alpha;
    assert!(matches!(
        ControlMap::try_new(undeclared_overlap),
        Err(ControlError::CoefficientTargetMismatch { .. })
    ));
}

#[test]
fn covariance_propagation_matches_independent_jacobian_calculation() {
    let (map, btc, _) = fixture();
    let covariance = FeatureCovariance::try_new(
        vec![key("signal_a"), key("signal_b")],
        vec![vec![4.0, 1.0], vec![1.0, 9.0]],
    )
    .expect("positive-definite feature covariance");
    let evaluation = map
        .evaluate_detailed(&btc, &complete_vector(), Some(&covariance))
        .expect("covariance propagation");
    let propagated = evaluation
        .propagated_covariance
        .expect("propagated covariance");

    assert_relative(propagated.alpha_variance, 18.0, 1e-13);
    assert_relative(propagated.alpha_beta_covariance, 3.05, 1e-13);
    assert_relative(propagated.beta_variance, 1.36, 1e-13);
}

#[test]
fn invalid_vectors_covariances_and_package_inputs_fail_closed() {
    let (map, btc, _) = fixture();
    assert!(ControlValue::present(key("signal_a"), f64::NAN).is_err());
    assert!(matches!(
        ControlVector::try_new(vec![
            ControlValue::present(key("signal_a"), 1.0).expect("feature"),
            ControlValue::present(key("signal_a"), 2.0).expect("feature"),
        ]),
        Err(ControlError::DuplicateFeature)
    ));
    assert!(
        FeatureCovariance::try_new(
            vec![key("signal_a"), key("signal_b")],
            vec![vec![1.0, 2.0], vec![2.0, 1.0]]
        )
        .is_err()
    );

    let unknown = ControlVector::try_new(vec![
        ControlValue::present(key("signal_a"), 2.0).expect("feature"),
        ControlValue::present(key("unknown"), 1.0).expect("feature"),
    ])
    .expect("bounded vector");
    assert!(matches!(
        map.evaluate(&btc, &unknown),
        Err(ControlError::FeatureSetMismatch)
    ));

    let mut zero_hash = map.into_input();
    zero_hash.normalization_hash = [0; 32];
    assert!(matches!(
        ControlMap::try_new(zero_hash),
        Err(ControlError::ZeroNormalizationHash)
    ));

    let (map, _, _) = fixture();
    let mut invalid_normalization = map.into_input();
    invalid_normalization.schema.features[0].normalization_scale = 0.0;
    assert!(matches!(
        ControlMap::try_new(invalid_normalization),
        Err(ControlError::InvalidNormalization)
    ));
}

#[test]
fn serialized_control_map_round_trips_through_semantic_validation() {
    let (map, _, _) = fixture();
    let encoded = serde_json::to_value(&map).expect("serialize map");
    let decoded: ControlMap = serde_json::from_value(encoded.clone()).expect("validated map");
    assert_eq!(decoded, map);

    let mut forged = encoded;
    forged["alpha"]["shared"][0]["coefficient"] = serde_json::json!(-1.0);
    assert!(serde_json::from_value::<ControlMap>(forged).is_err());
}

#[test]
fn schema_requires_exact_model_eligible_feature_registry_entries() {
    let (map, _, _) = fixture();
    let schema = map.into_input().schema;
    assert!(matches!(
        schema.validate_against_registry(&FeatureRegistry::new()),
        Err(ControlError::FeatureNotModelEligible)
    ));

    let mut registry = FeatureRegistry::new();
    registry
        .register(definition("signal_a", 1))
        .expect("register signal a");
    registry
        .register(definition("signal_b", 2))
        .expect("register signal b");
    schema
        .validate_against_registry(&registry)
        .expect("exact model-eligible definitions");
}

#[test]
fn explicit_neither_target_rejects_any_fitted_coefficient() {
    let schema = ControlSchema::try_new(
        Version::new(1, 0, 0),
        vec![
            FeatureGroup::try_new("audit", CoefficientSign::Any, CoefficientSign::Any, 0.0)
                .expect("group"),
        ],
        vec![
            ControlFeature::try_new(
                key("audit_only"),
                false,
                "audit",
                ControlTarget::Neither,
                0.0,
                1.0,
            )
            .expect("feature"),
        ],
    )
    .expect("explicit neither schema");
    let invalid = ControlMap::try_new(cusp::ControlMapInput {
        version: Version::new(1, 0, 0),
        schema,
        alpha: LinearControl::try_new(
            0.0,
            vec![FeatureCoefficient::try_new(key("audit_only"), 1.0).expect("coefficient")],
            BTreeMap::new(),
        )
        .expect("linear control"),
        beta: LinearControl::try_new(0.0, vec![], BTreeMap::new()).expect("linear control"),
        normalization_hash: [9; 32],
        residual_covariance: ControlCovariance::try_new(1.0, 0.0, 1.0).expect("covariance"),
    });
    assert!(matches!(
        invalid,
        Err(ControlError::CoefficientTargetMismatch { .. })
    ));
}

fn fixture() -> (ControlMap, AssetId, AssetId) {
    let btc_generation_one = asset(1);
    let btc_generation_two = asset(2);
    let groups = vec![
        FeatureGroup::try_new(
            "directional",
            CoefficientSign::NonNegative,
            CoefficientSign::NonNegative,
            0.2,
        )
        .expect("directional group"),
        FeatureGroup::try_new("optional", CoefficientSign::Any, CoefficientSign::Any, 0.1)
            .expect("optional group"),
    ];
    let features = vec![
        ControlFeature::try_new(
            key("signal_a"),
            true,
            "directional",
            ControlTarget::Both,
            1.0,
            2.0,
        )
        .expect("required feature"),
        ControlFeature::try_new(
            key("signal_b"),
            false,
            "optional",
            ControlTarget::Alpha,
            0.0,
            1.0,
        )
        .expect("optional feature"),
    ];
    let schema =
        ControlSchema::try_new(Version::new(1, 0, 0), groups, features).expect("control schema");
    let alpha = LinearControl::try_new(
        0.25,
        vec![
            FeatureCoefficient::try_new(key("signal_a"), 2.0).expect("coefficient"),
            FeatureCoefficient::try_new(key("signal_b"), 1.0).expect("coefficient"),
        ],
        BTreeMap::from([(
            btc_generation_one.clone(),
            vec![FeatureCoefficient::try_new(key("signal_a"), 0.5).expect("asset coefficient")],
        )]),
    )
    .expect("alpha mapping");
    let beta = LinearControl::try_new(
        0.5,
        vec![FeatureCoefficient::try_new(key("signal_a"), 1.0).expect("coefficient")],
        BTreeMap::new(),
    )
    .expect("beta mapping");
    let map = ControlMap::try_new(cusp::ControlMapInput {
        version: Version::new(1, 0, 0),
        schema,
        alpha,
        beta,
        normalization_hash: [7; 32],
        residual_covariance: ControlCovariance::try_new(0.25, 0.05, 0.36)
            .expect("residual covariance"),
    })
    .expect("valid map");
    (map, btc_generation_one, btc_generation_two)
}

fn complete_vector() -> ControlVector {
    ControlVector::try_new(vec![
        ControlValue::present(key("signal_a"), 2.0).expect("finite feature"),
        ControlValue {
            key: key("signal_b"),
            datum: ControlDatum::Present(0.5),
        },
    ])
    .expect("complete vector")
}

fn key(id: &str) -> ControlFeatureKey {
    ControlFeatureKey::try_new(id, Version::new(1, 0, 0)).expect("feature key")
}

fn asset(generation: u32) -> AssetId {
    AssetId::new(AssetNamespace::Native, "bitcoin", "", "BTC", generation).expect("asset")
}

fn definition(id: &str, hash_byte: u8) -> FeatureDefinition {
    FeatureDefinition::try_new(FeatureDefinitionInput {
        id: FeatureId::new(id).expect("feature id"),
        version: Version::new(1, 0, 0),
        status: FeatureStatus::Required,
        consumption_role: FeatureConsumptionRole::ModelEligible,
        value_type: FeatureValueType::Float64,
        entities: EntityScope::Asset,
        required_inputs: vec![InputRequirement::new("normalized.feature").expect("input")],
        event_time_policy: EventTimePolicy::SourceEventTime,
        windows: vec![
            WindowDefinition::try_new_time(
                WindowId::new("rolling_5m").expect("window id"),
                WindowKind::Sliding,
                DurationNanos::new(300_000_000_000),
                Some(DurationNanos::new(60_000_000_000)),
            )
            .expect("window"),
        ],
        output_resolution: DurationNanos::new(60_000_000_000),
        allowed_lateness: DurationNanos::new(5_000_000_000),
        time_to_live: DurationNanos::new(86_400_000_000_000),
        normalization: NormalizationPolicy::try_new(
            NormalizationKind::RobustRolling,
            Version::new(1, 0, 0),
        )
        .expect("normalization"),
        missingness: MissingnessPolicy::Explicit,
        quality_gate: QualityRequirement::try_new(
            QualityScore::from_millionths(900_000).expect("quality"),
            QualityScore::from_millionths(900_000).expect("coverage"),
            vec![SourceHealthState::Healthy],
        )
        .expect("quality gate"),
        formula_hash: FormulaHash::new([hash_byte; 32]).expect("formula hash"),
        documentation: FeatureDocumentation::new(format!(
            "docs/data-dictionary/features.md#{}",
            id.replace('_', "-")
        ))
        .expect("documentation"),
    })
    .expect("feature definition")
}

fn assert_relative(actual: f64, expected: f64, tolerance: f64) {
    let scale = actual.abs().max(expected.abs()).max(1.0);
    assert!(
        (actual - expected).abs() <= tolerance * scale,
        "expected {expected}, got {actual}"
    );
}
