use std::collections::BTreeSet;

use ed25519_dalek::{Signature, VerifyingKey};
use semver::Version;
use serde::Serialize;
use thiserror::Error;

use crate::package::{SignedModelPackage, canonical_manifest, signing_message, validate_manifest};

/// A trusted key supplied by runtime policy, never by the package being checked.
pub struct TrustedVerifyingKey {
    key_id: String,
    key: VerifyingKey,
}

impl TrustedVerifyingKey {
    pub fn new(
        key_id: impl Into<String>,
        key: VerifyingKey,
    ) -> Result<Self, PackageVerificationError> {
        let key_id = key_id.into();
        if key_id.is_empty()
            || key_id.len() > 256
            || key_id.bytes().any(|byte| byte.is_ascii_whitespace())
        {
            return Err(PackageVerificationError::UntrustedKey);
        }
        Ok(Self { key_id, key })
    }
}

/// Opaque evidence that package structure, content address, artifact, and
/// signature were verified together.
#[derive(Clone)]
pub struct VerifiedPackage {
    package: SignedModelPackage,
}

impl VerifiedPackage {
    pub fn package(&self) -> &SignedModelPackage {
        &self.package
    }

    pub fn model_id(&self) -> &str {
        &self.package.manifest.model_id
    }

    pub const fn package_blake3(&self) -> [u8; 32] {
        self.package.package_blake3
    }
}

/// Runtime and request context that must exactly match a package.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompatibilityRequest {
    pub asset: String,
    pub event_type: String,
    pub horizon_seconds: u64,
    pub feature_schema_hash: [u8; 32],
    pub normalization_hash: [u8; 32],
    pub label_definition_hash: [u8; 32],
    pub runtime_version: Version,
    pub runtime_capabilities: BTreeSet<String>,
    pub available_memory_bytes: u64,
    pub observed_quality_millionths: u32,
    pub observed_source_count: u16,
    pub evaluated_at_ns: i64,
}

/// Opaque package-specific compatibility receipt.
#[derive(Clone)]
pub struct CompatiblePackage {
    verified: VerifiedPackage,
    request_blake3: [u8; 32],
}

impl CompatiblePackage {
    pub fn model_id(&self) -> &str {
        self.verified.model_id()
    }

    pub const fn package_blake3(&self) -> [u8; 32] {
        self.verified.package_blake3()
    }

    pub const fn request_blake3(&self) -> [u8; 32] {
        self.request_blake3
    }
}

pub fn verify_package(
    package: &SignedModelPackage,
    artifact: &[u8],
    trusted_key: &TrustedVerifyingKey,
) -> Result<VerifiedPackage, PackageVerificationError> {
    if package.signing_key_id != trusted_key.key_id {
        return Err(PackageVerificationError::UntrustedKey);
    }
    validate_manifest(&package.manifest).map_err(PackageVerificationError::InvalidManifest)?;
    if package.manifest.artifact_blake3 != *blake3::hash(artifact).as_bytes() {
        return Err(PackageVerificationError::ArtifactMismatch);
    }
    let canonical =
        canonical_manifest(&package.manifest).map_err(PackageVerificationError::InvalidManifest)?;
    if package.package_blake3 != *blake3::hash(&canonical).as_bytes() {
        return Err(PackageVerificationError::ContentAddressMismatch);
    }
    let signature = Signature::try_from(package.signature.as_slice())
        .map_err(|_| PackageVerificationError::InvalidSignature)?;
    trusted_key
        .key
        .verify_strict(
            &signing_message(&package.signing_key_id, &canonical),
            &signature,
        )
        .map_err(|_| PackageVerificationError::InvalidSignature)?;
    Ok(VerifiedPackage {
        package: package.clone(),
    })
}

pub fn verify_compatibility(
    package: &VerifiedPackage,
    request: &CompatibilityRequest,
) -> Result<CompatiblePackage, PackageVerificationError> {
    let manifest = &package.package.manifest;
    if request.horizon_seconds == 0
        || request.available_memory_bytes == 0
        || request.observed_quality_millionths > 1_000_000
        || request.observed_source_count == 0
        || request.asset.is_empty()
        || request.event_type.is_empty()
        || request.runtime_capabilities.is_empty()
    {
        return Err(PackageVerificationError::InvalidRequest);
    }
    if request.evaluated_at_ns <= 0 || request.evaluated_at_ns >= manifest.expires_at_ns {
        return Err(PackageVerificationError::Expired);
    }
    if request.evaluated_at_ns >= manifest.review_after_ns {
        return Err(PackageVerificationError::ReviewRequired);
    }
    if !manifest.supported_assets.contains(&request.asset) {
        return Err(PackageVerificationError::AssetMismatch);
    }
    if !manifest.supported_event_types.contains(&request.event_type) {
        return Err(PackageVerificationError::EventTypeMismatch);
    }
    if !manifest
        .supported_horizons_seconds
        .contains(&request.horizon_seconds)
    {
        return Err(PackageVerificationError::HorizonMismatch);
    }
    if request.feature_schema_hash != manifest.feature_schema_hash {
        return Err(PackageVerificationError::FeatureSchemaMismatch);
    }
    if request.normalization_hash != manifest.normalization_hash {
        return Err(PackageVerificationError::NormalizationMismatch);
    }
    if request.label_definition_hash != manifest.label_definition_hash {
        return Err(PackageVerificationError::LabelMismatch);
    }
    if request.runtime_version < manifest.runtime_requirements.minimum_runtime_version {
        return Err(PackageVerificationError::RuntimeVersionMismatch);
    }
    if !manifest
        .runtime_requirements
        .required_capabilities
        .iter()
        .all(|capability| request.runtime_capabilities.contains(capability))
    {
        return Err(PackageVerificationError::CapabilityMismatch);
    }
    if request.available_memory_bytes < manifest.runtime_requirements.minimum_memory_bytes {
        return Err(PackageVerificationError::MemoryMismatch);
    }
    if request.observed_quality_millionths
        < manifest.quality_requirements.minimum_quality_millionths
        || request.observed_source_count < manifest.quality_requirements.minimum_source_count
    {
        return Err(PackageVerificationError::QualityMismatch);
    }
    let request_wire = serde_json::to_vec(&CompatibilityWire {
        asset: &request.asset,
        event_type: &request.event_type,
        horizon_seconds: request.horizon_seconds,
        feature_schema_hash: request.feature_schema_hash,
        normalization_hash: request.normalization_hash,
        label_definition_hash: request.label_definition_hash,
        runtime_version: &request.runtime_version,
        runtime_capabilities: &request.runtime_capabilities,
        available_memory_bytes: request.available_memory_bytes,
        observed_quality_millionths: request.observed_quality_millionths,
        observed_source_count: request.observed_source_count,
        evaluated_at_ns: request.evaluated_at_ns,
    })
    .map_err(PackageVerificationError::CompatibilityEncoding)?;
    Ok(CompatiblePackage {
        verified: package.clone(),
        request_blake3: *blake3::hash(&request_wire).as_bytes(),
    })
}

#[derive(Serialize)]
struct CompatibilityWire<'a> {
    asset: &'a str,
    event_type: &'a str,
    horizon_seconds: u64,
    feature_schema_hash: [u8; 32],
    normalization_hash: [u8; 32],
    label_definition_hash: [u8; 32],
    runtime_version: &'a Version,
    runtime_capabilities: &'a BTreeSet<String>,
    available_memory_bytes: u64,
    observed_quality_millionths: u32,
    observed_source_count: u16,
    evaluated_at_ns: i64,
}

#[derive(Debug, Error)]
pub enum PackageVerificationError {
    #[error("package manifest is invalid")]
    InvalidManifest(#[source] crate::package::PackageBuildError),
    #[error("package signing key is not trusted")]
    UntrustedKey,
    #[error("artifact digest does not match the signed manifest")]
    ArtifactMismatch,
    #[error("package content address does not match the canonical manifest")]
    ContentAddressMismatch,
    #[error("package signature is invalid")]
    InvalidSignature,
    #[error("compatibility request is invalid")]
    InvalidRequest,
    #[error("compatibility request could not be encoded")]
    CompatibilityEncoding(#[source] serde_json::Error),
    #[error("package is expired or evaluation time is invalid")]
    Expired,
    #[error("package is due for review")]
    ReviewRequired,
    #[error("asset is unsupported")]
    AssetMismatch,
    #[error("event type is unsupported")]
    EventTypeMismatch,
    #[error("horizon is unsupported")]
    HorizonMismatch,
    #[error("feature schema is incompatible")]
    FeatureSchemaMismatch,
    #[error("normalization schema is incompatible")]
    NormalizationMismatch,
    #[error("label definition is incompatible")]
    LabelMismatch,
    #[error("runtime version is incompatible")]
    RuntimeVersionMismatch,
    #[error("runtime capability is missing")]
    CapabilityMismatch,
    #[error("runtime memory is insufficient")]
    MemoryMismatch,
    #[error("observed input quality is insufficient")]
    QualityMismatch,
}
