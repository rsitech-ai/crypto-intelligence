use std::{
    collections::{BTreeSet, HashSet},
    fs,
    path::Path,
};

use ed25519_dalek::SigningKey;
use model_registry::{
    CalibrationDescriptor, CompatibilityRequest, IndependentReview, MetricSummary,
    ModelPackageManifestInput, ModelRegistry, ModelState, PackagePeriod, PackageSigner,
    PackageVerificationError, PromotionError, QualityRequirements, RuntimeRequirements,
    ShadowEvidence, TrustedVerifyingKey, verify_compatibility, verify_package,
};
use semver::Version;

const ARTIFACT: &[u8] = b"deterministic-model-artifact-v1";

fn manifest() -> ModelPackageManifestInput {
    ModelPackageManifestInput {
        schema_version: 1,
        model_id: "btc-hazard-v1".into(),
        semantic_version: Version::new(1, 0, 0),
        model_family: "competing-risk-hazard".into(),
        supported_assets: vec!["BTC".into()],
        supported_event_types: vec!["drawdown".into()],
        supported_horizons_seconds: vec![900, 3_600],
        training_period: PackagePeriod::new(100, 200).expect("training period"),
        validation_period: PackagePeriod::new(300, 400).expect("validation period"),
        outer_test_period: PackagePeriod::new(500, 600).expect("test period"),
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
            validation_period: PackagePeriod::new(300, 400).expect("validation period"),
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
            minimum_runtime_version: Version::new(1, 2, 0),
            required_capabilities: vec!["hazard-v1".into(), "point-in-time-v1".into()],
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
        review_after_ns: 700,
        expires_at_ns: 900,
    }
}

fn signed_fixture() -> (model_registry::SignedModelPackage, TrustedVerifyingKey) {
    let signing_key = SigningKey::from_bytes(&[0x42; 32]);
    let trusted_key = TrustedVerifyingKey::new("fixture-key-v1", signing_key.verifying_key())
        .expect("trusted key");
    let package = PackageSigner::sign(manifest(), ARTIFACT, "fixture-key-v1", &signing_key)
        .expect("signed package");
    (package, trusted_key)
}

fn compatibility_request() -> CompatibilityRequest {
    CompatibilityRequest {
        asset: "BTC".into(),
        event_type: "drawdown".into(),
        horizon_seconds: 900,
        feature_schema_hash: [2; 32],
        normalization_hash: [3; 32],
        label_definition_hash: [4; 32],
        runtime_version: Version::new(1, 3, 0),
        runtime_capabilities: BTreeSet::from(["hazard-v1".into(), "point-in-time-v1".into()]),
        available_memory_bytes: 2_048,
        observed_quality_millionths: 950_000,
        observed_source_count: 3,
        evaluated_at_ns: 650,
    }
}

fn compatible(verified: &model_registry::VerifiedPackage) -> model_registry::CompatiblePackage {
    verify_compatibility(verified, &compatibility_request()).expect("compatible fixture")
}

#[test]
fn signature_binds_manifest_artifact_and_external_trust_key() {
    let (package, trusted_key) = signed_fixture();
    let verified = verify_package(&package, ARTIFACT, &trusted_key).expect("verified fixture");
    assert_eq!(verified.model_id(), "btc-hazard-v1");

    assert!(matches!(
        verify_package(&package, b"tampered-artifact", &trusted_key),
        Err(PackageVerificationError::ArtifactMismatch)
    ));

    let mut tampered = package.clone();
    tampered.manifest.limitations[0].push_str("-tampered");
    assert!(matches!(
        verify_package(&tampered, ARTIFACT, &trusted_key),
        Err(PackageVerificationError::ContentAddressMismatch)
    ));

    let other_key = SigningKey::from_bytes(&[0x24; 32]);
    let untrusted =
        TrustedVerifyingKey::new("other-key", other_key.verifying_key()).expect("other key");
    assert!(matches!(
        verify_package(&package, ARTIFACT, &untrusted),
        Err(PackageVerificationError::UntrustedKey)
    ));

    let signing_key = SigningKey::from_bytes(&[0x42; 32]);
    let alias_key = TrustedVerifyingKey::new("fixture-key-alias", signing_key.verifying_key())
        .expect("alias key");
    let mut aliased = package;
    aliased.signing_key_id = "fixture-key-alias".into();
    assert!(matches!(
        verify_package(&aliased, ARTIFACT, &alias_key),
        Err(PackageVerificationError::InvalidSignature)
    ));
}

#[test]
fn compatibility_checks_every_material_runtime_boundary() {
    let (package, trusted_key) = signed_fixture();
    let verified = verify_package(&package, ARTIFACT, &trusted_key).expect("verified fixture");
    let valid = compatible(&verified);
    assert_eq!(valid.model_id(), "btc-hazard-v1");

    let assert_error = |request: &CompatibilityRequest, expected: PackageVerificationError| {
        let actual = match verify_compatibility(&verified, request) {
            Err(error) => error,
            Ok(_) => panic!("must be incompatible"),
        };
        assert_eq!(
            std::mem::discriminant(&actual),
            std::mem::discriminant(&expected)
        );
    };

    let mut request = compatibility_request();
    request.feature_schema_hash = [0x55; 32];
    assert_error(&request, PackageVerificationError::FeatureSchemaMismatch);
    request = compatibility_request();
    request.normalization_hash = [0x55; 32];
    assert_error(&request, PackageVerificationError::NormalizationMismatch);
    request = compatibility_request();
    request.label_definition_hash = [0x55; 32];
    assert_error(&request, PackageVerificationError::LabelMismatch);
    request = compatibility_request();
    request.runtime_version = Version::new(1, 1, 9);
    assert_error(&request, PackageVerificationError::RuntimeVersionMismatch);
    request = compatibility_request();
    request.runtime_capabilities.remove("hazard-v1");
    assert_error(&request, PackageVerificationError::CapabilityMismatch);
    request = compatibility_request();
    request.available_memory_bytes = 1_023;
    assert_error(&request, PackageVerificationError::MemoryMismatch);
    request = compatibility_request();
    request.observed_quality_millionths = 899_999;
    assert_error(&request, PackageVerificationError::QualityMismatch);
    request = compatibility_request();
    request.evaluated_at_ns = 700;
    assert_error(&request, PackageVerificationError::ReviewRequired);
    request = compatibility_request();
    request.evaluated_at_ns = 900;
    assert_error(&request, PackageVerificationError::Expired);
}

#[test]
fn json_schema_tracks_the_signed_wire_shape() {
    let (package, _) = signed_fixture();
    let package_json = serde_json::to_value(package).expect("package JSON");
    let schema_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../models/schemas/model-package.schema.json");
    let schema: serde_json::Value =
        serde_json::from_slice(&fs::read(schema_path).expect("schema file")).expect("schema JSON");
    assert_eq!(
        schema["$schema"],
        "https://json-schema.org/draft/2020-12/schema"
    );
    assert_eq!(schema["additionalProperties"], false);
    assert_eq!(schema["$defs"]["manifest"]["additionalProperties"], false);

    let package_keys = package_json
        .as_object()
        .expect("package object")
        .keys()
        .cloned()
        .collect::<HashSet<_>>();
    let root_schema_keys = schema["properties"]
        .as_object()
        .expect("root schema properties")
        .keys()
        .cloned()
        .collect::<HashSet<_>>();
    assert_eq!(package_keys, root_schema_keys);

    let manifest_keys = package_json["manifest"]
        .as_object()
        .expect("manifest object")
        .keys()
        .cloned()
        .collect::<HashSet<_>>();
    let manifest_schema_keys = schema["$defs"]["manifest"]["properties"]
        .as_object()
        .expect("manifest schema properties")
        .keys()
        .cloned()
        .collect::<HashSet<_>>();
    assert_eq!(manifest_keys, manifest_schema_keys);
}

#[test]
fn production_promotion_requires_signature_metrics_shadow_and_independent_review() {
    let (package, trusted_key) = signed_fixture();
    let verified = verify_package(&package, ARTIFACT, &trusted_key).expect("verified fixture");
    let compatibility = compatible(&verified);
    let package_hash = verified.package_blake3();

    let mut registry = ModelRegistry::new();
    registry
        .register_research(verified, "model-owner", "training-run-1", 610)
        .expect("register");
    let candidate_review =
        IndependentReview::new("model-owner", "reviewer-a", "candidate-review", 620)
            .expect("candidate review");
    registry
        .promote_candidate("btc-hazard-v1", &candidate_review)
        .expect("candidate");
    let shadow_review = IndependentReview::new("model-owner", "reviewer-b", "shadow-review", 630)
        .expect("shadow review");
    registry
        .promote_shadow("btc-hazard-v1", &shadow_review)
        .expect("shadow");

    assert!(matches!(
        registry.promote_production("btc-hazard-v1", Some(&compatibility), None, None),
        Err(PromotionError::MissingIndependentReview)
    ));
    let production_review =
        IndependentReview::new("model-owner", "reviewer-c", "production-review", 660)
            .expect("production review");
    let shadow =
        ShadowEvidence::new(package_hash, "shadow-soak-1", 100, 8, 640, 650).expect("shadow");
    registry
        .promote_production(
            "btc-hazard-v1",
            Some(&compatibility),
            Some(&production_review),
            Some(&shadow),
        )
        .expect("production promotion");
    assert!(registry.may_infer_production("btc-hazard-v1"));
    assert_eq!(
        registry.get("btc-hazard-v1").expect("record").state(),
        ModelState::Production
    );
}

#[test]
fn revoked_model_cannot_issue_new_forecasts_but_remains_auditable() {
    let (package, trusted_key) = signed_fixture();
    let verified = verify_package(&package, ARTIFACT, &trusted_key).expect("verified fixture");
    let mut registry = ModelRegistry::new();
    registry
        .register_research(verified, "model-owner", "training-run-1", 610)
        .expect("register");
    registry
        .revoke("btc-hazard-v1", "security-owner", "incident-17", 620)
        .expect("revoke");

    assert!(!registry.may_infer_production("btc-hazard-v1"));
    let record = registry.get("btc-hazard-v1").expect("audit record");
    assert_eq!(record.state(), ModelState::Revoked);
    assert_eq!(record.history().len(), 2);
    assert_eq!(record.history()[1].evidence_id, "incident-17");
}
