use ed25519_dalek::{Signer as _, SigningKey};
use semver::Version;
use serde::{Deserialize, Serialize};
use thiserror::Error;

const SIGNING_DOMAIN: &[u8] = b"crypto-intelligence-model-package-v1\0";
const MAXIMUM_ARTIFACT_BYTES: usize = 512 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PackagePeriod {
    pub start_ns: i64,
    pub end_ns: i64,
}

impl PackagePeriod {
    pub fn new(start_ns: i64, end_ns: i64) -> Result<Self, PackageBuildError> {
        if start_ns <= 0 || end_ns <= start_ns {
            return Err(PackageBuildError::InvalidPeriod);
        }
        Ok(Self { start_ns, end_ns })
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CalibrationDescriptor {
    pub method: String,
    pub version: String,
    pub evidence_blake3: [u8; 32],
    pub validation_period: PackagePeriod,
    pub effective_sample_size: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetricSummary {
    pub outer_test_evidence_blake3: [u8; 32],
    pub observations: u64,
    pub positives: u64,
    pub log_loss: f64,
    pub brier_skill_score: f64,
    pub calibration_intercept: f64,
    pub calibration_slope: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QualityRequirements {
    pub minimum_quality_millionths: u32,
    pub minimum_source_count: u16,
    pub minimum_outer_test_positives: u64,
    pub minimum_shadow_positives: u64,
    pub minimum_brier_skill_score: f64,
    pub maximum_log_loss: f64,
    pub calibration_slope_minimum: f64,
    pub calibration_slope_maximum: f64,
    pub calibration_intercept_absolute_maximum: f64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeRequirements {
    pub minimum_runtime_version: Version,
    pub required_capabilities: Vec<String>,
    pub minimum_memory_bytes: u64,
}

/// Material metadata supplied before the artifact digest is known.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelPackageManifestInput {
    pub schema_version: u32,
    pub model_id: String,
    pub semantic_version: Version,
    pub model_family: String,
    pub supported_assets: Vec<String>,
    pub supported_event_types: Vec<String>,
    pub supported_horizons_seconds: Vec<u64>,
    pub training_period: PackagePeriod,
    pub validation_period: PackagePeriod,
    pub outer_test_period: PackagePeriod,
    pub data_hash: [u8; 32],
    pub feature_schema_hash: [u8; 32],
    pub normalization_hash: [u8; 32],
    pub label_definition_hash: [u8; 32],
    pub code_commit: String,
    pub hyperparameters_blake3: [u8; 32],
    pub calibration: CalibrationDescriptor,
    pub quality_requirements: QualityRequirements,
    pub runtime_requirements: RuntimeRequirements,
    pub metrics: MetricSummary,
    pub subgroup_metrics_blake3: [u8; 32],
    pub limitations: Vec<String>,
    pub model_card_blake3: [u8; 32],
    pub review_after_ns: i64,
    pub expires_at_ns: i64,
}

/// Versioned canonical package metadata. Its artifact hash is signature-bound.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelPackageManifest {
    pub schema_version: u32,
    pub model_id: String,
    pub semantic_version: Version,
    pub model_family: String,
    pub supported_assets: Vec<String>,
    pub supported_event_types: Vec<String>,
    pub supported_horizons_seconds: Vec<u64>,
    pub training_period: PackagePeriod,
    pub validation_period: PackagePeriod,
    pub outer_test_period: PackagePeriod,
    pub data_hash: [u8; 32],
    pub feature_schema_hash: [u8; 32],
    pub normalization_hash: [u8; 32],
    pub label_definition_hash: [u8; 32],
    pub code_commit: String,
    pub hyperparameters_blake3: [u8; 32],
    pub calibration: CalibrationDescriptor,
    pub quality_requirements: QualityRequirements,
    pub runtime_requirements: RuntimeRequirements,
    pub metrics: MetricSummary,
    pub subgroup_metrics_blake3: [u8; 32],
    pub limitations: Vec<String>,
    pub model_card_blake3: [u8; 32],
    pub review_after_ns: i64,
    pub expires_at_ns: i64,
    pub artifact_blake3: [u8; 32],
}

/// An immutable manifest plus key identifier and Ed25519 signature.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedModelPackage {
    pub manifest: ModelPackageManifest,
    pub package_blake3: [u8; 32],
    pub signing_key_id: String,
    pub signature: Vec<u8>,
}

/// Stateless signing boundary. The caller retains ownership of secret key
/// material; neither the key nor its bytes are stored or formatted here.
pub struct PackageSigner;

impl PackageSigner {
    pub fn sign(
        input: ModelPackageManifestInput,
        artifact: &[u8],
        signing_key_id: impl Into<String>,
        signing_key: &SigningKey,
    ) -> Result<SignedModelPackage, PackageBuildError> {
        validate_input(&input, artifact)?;
        let signing_key_id = signing_key_id.into();
        validate_identifier("signing_key_id", &signing_key_id)?;
        let manifest = ModelPackageManifest {
            schema_version: input.schema_version,
            model_id: input.model_id,
            semantic_version: input.semantic_version,
            model_family: input.model_family,
            supported_assets: input.supported_assets,
            supported_event_types: input.supported_event_types,
            supported_horizons_seconds: input.supported_horizons_seconds,
            training_period: input.training_period,
            validation_period: input.validation_period,
            outer_test_period: input.outer_test_period,
            data_hash: input.data_hash,
            feature_schema_hash: input.feature_schema_hash,
            normalization_hash: input.normalization_hash,
            label_definition_hash: input.label_definition_hash,
            code_commit: input.code_commit,
            hyperparameters_blake3: input.hyperparameters_blake3,
            calibration: input.calibration,
            quality_requirements: input.quality_requirements,
            runtime_requirements: input.runtime_requirements,
            metrics: input.metrics,
            subgroup_metrics_blake3: input.subgroup_metrics_blake3,
            limitations: input.limitations,
            model_card_blake3: input.model_card_blake3,
            review_after_ns: input.review_after_ns,
            expires_at_ns: input.expires_at_ns,
            artifact_blake3: *blake3::hash(artifact).as_bytes(),
        };
        let canonical = canonical_manifest(&manifest)?;
        let package_blake3 = *blake3::hash(&canonical).as_bytes();
        let message = signing_message(&signing_key_id, &canonical);
        let signature = signing_key.sign(&message).to_bytes().to_vec();
        Ok(SignedModelPackage {
            manifest,
            package_blake3,
            signing_key_id,
            signature,
        })
    }
}

pub(crate) fn canonical_manifest(
    manifest: &ModelPackageManifest,
) -> Result<Vec<u8>, PackageBuildError> {
    serde_json::to_vec(manifest).map_err(PackageBuildError::CanonicalEncoding)
}

pub(crate) fn signing_message(signing_key_id: &str, canonical: &[u8]) -> Vec<u8> {
    let mut message =
        Vec::with_capacity(SIGNING_DOMAIN.len() + signing_key_id.len() + canonical.len() + 1);
    message.extend_from_slice(SIGNING_DOMAIN);
    message.extend_from_slice(signing_key_id.as_bytes());
    message.push(0);
    message.extend_from_slice(canonical);
    message
}

pub(crate) fn validate_manifest(manifest: &ModelPackageManifest) -> Result<(), PackageBuildError> {
    validate_input_fields(
        manifest.schema_version,
        &manifest.model_id,
        &manifest.semantic_version,
        &manifest.model_family,
        &manifest.supported_assets,
        &manifest.supported_event_types,
        &manifest.supported_horizons_seconds,
        manifest.training_period,
        manifest.validation_period,
        manifest.outer_test_period,
        &manifest.data_hash,
        &manifest.feature_schema_hash,
        &manifest.normalization_hash,
        &manifest.label_definition_hash,
        &manifest.code_commit,
        &manifest.hyperparameters_blake3,
        &manifest.calibration,
        &manifest.quality_requirements,
        &manifest.runtime_requirements,
        &manifest.metrics,
        &manifest.subgroup_metrics_blake3,
        &manifest.limitations,
        &manifest.model_card_blake3,
        manifest.review_after_ns,
        manifest.expires_at_ns,
    )?;
    require_hash("artifact_blake3", &manifest.artifact_blake3)
}

fn validate_input(
    input: &ModelPackageManifestInput,
    artifact: &[u8],
) -> Result<(), PackageBuildError> {
    if artifact.is_empty() || artifact.len() > MAXIMUM_ARTIFACT_BYTES {
        return Err(PackageBuildError::ArtifactCapacity);
    }
    validate_input_fields(
        input.schema_version,
        &input.model_id,
        &input.semantic_version,
        &input.model_family,
        &input.supported_assets,
        &input.supported_event_types,
        &input.supported_horizons_seconds,
        input.training_period,
        input.validation_period,
        input.outer_test_period,
        &input.data_hash,
        &input.feature_schema_hash,
        &input.normalization_hash,
        &input.label_definition_hash,
        &input.code_commit,
        &input.hyperparameters_blake3,
        &input.calibration,
        &input.quality_requirements,
        &input.runtime_requirements,
        &input.metrics,
        &input.subgroup_metrics_blake3,
        &input.limitations,
        &input.model_card_blake3,
        input.review_after_ns,
        input.expires_at_ns,
    )
}

#[allow(clippy::too_many_arguments)]
fn validate_input_fields(
    schema_version: u32,
    model_id: &str,
    semantic_version: &Version,
    model_family: &str,
    supported_assets: &[String],
    supported_event_types: &[String],
    supported_horizons_seconds: &[u64],
    training_period: PackagePeriod,
    validation_period: PackagePeriod,
    outer_test_period: PackagePeriod,
    data_hash: &[u8; 32],
    feature_schema_hash: &[u8; 32],
    normalization_hash: &[u8; 32],
    label_definition_hash: &[u8; 32],
    code_commit: &str,
    hyperparameters_blake3: &[u8; 32],
    calibration: &CalibrationDescriptor,
    quality: &QualityRequirements,
    runtime: &RuntimeRequirements,
    metrics: &MetricSummary,
    subgroup_metrics_blake3: &[u8; 32],
    limitations: &[String],
    model_card_blake3: &[u8; 32],
    review_after_ns: i64,
    expires_at_ns: i64,
) -> Result<(), PackageBuildError> {
    if schema_version != 1 || semantic_version.major == 0 {
        return Err(PackageBuildError::UnsupportedVersion);
    }
    validate_identifier("model_id", model_id)?;
    validate_identifier("model_family", model_family)?;
    validate_unique_strings("supported_assets", supported_assets)?;
    validate_unique_strings("supported_event_types", supported_event_types)?;
    validate_unique_u64("supported_horizons_seconds", supported_horizons_seconds)?;
    if [training_period, validation_period, outer_test_period]
        .into_iter()
        .any(|period| period.start_ns <= 0 || period.end_ns <= period.start_ns)
    {
        return Err(PackageBuildError::InvalidPeriod);
    }
    if training_period.end_ns >= validation_period.start_ns
        || validation_period.end_ns >= outer_test_period.start_ns
    {
        return Err(PackageBuildError::PeriodLeakage);
    }
    for (field, hash) in [
        ("data_hash", data_hash),
        ("feature_schema_hash", feature_schema_hash),
        ("normalization_hash", normalization_hash),
        ("label_definition_hash", label_definition_hash),
        ("hyperparameters_blake3", hyperparameters_blake3),
        ("subgroup_metrics_blake3", subgroup_metrics_blake3),
        ("model_card_blake3", model_card_blake3),
    ] {
        require_hash(field, hash)?;
    }
    if code_commit.len() != 40
        || !code_commit
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(PackageBuildError::InvalidField("code_commit"));
    }
    validate_identifier("calibration.method", &calibration.method)?;
    validate_identifier("calibration.version", &calibration.version)?;
    require_hash("calibration.evidence_blake3", &calibration.evidence_blake3)?;
    if calibration.validation_period != validation_period || calibration.effective_sample_size < 8 {
        return Err(PackageBuildError::InvalidCalibration);
    }
    if quality.minimum_quality_millionths > 1_000_000
        || quality.minimum_source_count == 0
        || quality.minimum_outer_test_positives == 0
        || quality.minimum_shadow_positives == 0
        || ![
            quality.minimum_brier_skill_score,
            quality.maximum_log_loss,
            quality.calibration_slope_minimum,
            quality.calibration_slope_maximum,
            quality.calibration_intercept_absolute_maximum,
        ]
        .into_iter()
        .all(f64::is_finite)
        || quality.maximum_log_loss <= 0.0
        || quality.minimum_brier_skill_score > 1.0
        || quality.calibration_slope_minimum <= 0.0
        || quality.calibration_slope_maximum < quality.calibration_slope_minimum
        || quality.calibration_intercept_absolute_maximum < 0.0
    {
        return Err(PackageBuildError::InvalidQuality);
    }
    if runtime.minimum_runtime_version.major == 0
        || runtime.minimum_memory_bytes == 0
        || runtime.required_capabilities.is_empty()
    {
        return Err(PackageBuildError::InvalidRuntime);
    }
    validate_unique_strings("required_capabilities", &runtime.required_capabilities)?;
    require_hash(
        "metrics.outer_test_evidence_blake3",
        &metrics.outer_test_evidence_blake3,
    )?;
    if metrics.observations < metrics.positives
        || metrics.positives < quality.minimum_outer_test_positives
        || ![
            metrics.log_loss,
            metrics.brier_skill_score,
            metrics.calibration_intercept,
            metrics.calibration_slope,
        ]
        .into_iter()
        .all(f64::is_finite)
        || metrics.log_loss > quality.maximum_log_loss
        || metrics.log_loss < 0.0
        || metrics.brier_skill_score > 1.0
        || metrics.brier_skill_score < quality.minimum_brier_skill_score
        || metrics.calibration_slope < quality.calibration_slope_minimum
        || metrics.calibration_slope > quality.calibration_slope_maximum
        || metrics.calibration_intercept.abs() > quality.calibration_intercept_absolute_maximum
    {
        return Err(PackageBuildError::InvalidMetrics);
    }
    if limitations.is_empty() || limitations.len() > 64 {
        return Err(PackageBuildError::InvalidLimitations);
    }
    for limitation in limitations {
        validate_text("limitations", limitation, 1_024)?;
    }
    if review_after_ns <= outer_test_period.end_ns || expires_at_ns <= review_after_ns {
        return Err(PackageBuildError::InvalidExpiry);
    }
    Ok(())
}

fn validate_unique_strings(
    field: &'static str,
    values: &[String],
) -> Result<(), PackageBuildError> {
    if values.is_empty() || values.len() > 256 {
        return Err(PackageBuildError::InvalidField(field));
    }
    let mut previous: Option<&str> = None;
    for value in values {
        validate_identifier(field, value)?;
        if previous.is_some_and(|item| item >= value.as_str()) {
            return Err(PackageBuildError::InvalidField(field));
        }
        previous = Some(value);
    }
    Ok(())
}

fn validate_unique_u64(field: &'static str, values: &[u64]) -> Result<(), PackageBuildError> {
    if values.is_empty()
        || values.len() > 256
        || values.contains(&0)
        || values.windows(2).any(|pair| pair[0] >= pair[1])
    {
        return Err(PackageBuildError::InvalidField(field));
    }
    Ok(())
}

fn validate_identifier(field: &'static str, value: &str) -> Result<(), PackageBuildError> {
    validate_text(field, value, 256)?;
    if value.bytes().any(|byte| byte.is_ascii_whitespace()) {
        return Err(PackageBuildError::InvalidField(field));
    }
    Ok(())
}

fn validate_text(
    field: &'static str,
    value: &str,
    maximum: usize,
) -> Result<(), PackageBuildError> {
    if value.is_empty()
        || value.len() > maximum
        || value.bytes().any(|byte| byte.is_ascii_control())
    {
        return Err(PackageBuildError::InvalidField(field));
    }
    Ok(())
}

fn require_hash(field: &'static str, value: &[u8; 32]) -> Result<(), PackageBuildError> {
    if *value == [0; 32] {
        Err(PackageBuildError::InvalidField(field))
    } else {
        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum PackageBuildError {
    #[error("package period is invalid")]
    InvalidPeriod,
    #[error("package schema or semantic version is unsupported")]
    UnsupportedVersion,
    #[error("package field `{0}` is invalid")]
    InvalidField(&'static str),
    #[error("training, validation, and outer test periods overlap")]
    PeriodLeakage,
    #[error("calibration descriptor is invalid")]
    InvalidCalibration,
    #[error("quality requirements are invalid")]
    InvalidQuality,
    #[error("runtime requirements are invalid")]
    InvalidRuntime,
    #[error("outer-test metrics are invalid")]
    InvalidMetrics,
    #[error("model limitations are invalid")]
    InvalidLimitations,
    #[error("review and expiry boundaries are invalid")]
    InvalidExpiry,
    #[error("artifact size is outside the package contract")]
    ArtifactCapacity,
    #[error("canonical package encoding failed")]
    CanonicalEncoding(#[source] serde_json::Error),
}
