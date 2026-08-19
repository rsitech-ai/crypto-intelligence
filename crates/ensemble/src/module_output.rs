//! Point-in-time module output and availability contracts.

use std::collections::BTreeMap;

use dataset::OuterFold;
use domain::UnixNanos;
use feature_registry::{FeatureEntity, FeatureId, FiniteF64, MissingnessReason, QualityScore};
use model_registry::VerifiedPackage;

use crate::{
    EnsembleError, hash_i64, hash_string, hash_u64, require_nonzero_digest, valid_identifier,
};

const MAXIMUM_VALUES_PER_MODULE: usize = 256;
const MODULE_OUTPUT_DOMAIN: &[u8] = b"cmti:ensemble-module-output:v2\0";

/// Closed Phase 4 module inventory used by the calibrated ensemble.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum ModuleKind {
    BaseRate,
    Volatility,
    Changepoint,
    Regime,
    Hazard,
    Cusp,
}

impl ModuleKind {
    pub const ALL: [Self; 6] = [
        Self::BaseRate,
        Self::Volatility,
        Self::Changepoint,
        Self::Regime,
        Self::Hazard,
        Self::Cusp,
    ];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::BaseRate => "base_rate",
            Self::Volatility => "volatility",
            Self::Changepoint => "changepoint",
            Self::Regime => "regime",
            Self::Hazard => "hazard",
            Self::Cusp => "cusp",
        }
    }

    pub(crate) const fn identity_tag(self) -> u8 {
        match self {
            Self::BaseRate => 1,
            Self::Volatility => 2,
            Self::Changepoint => 3,
            Self::Regime => 4,
            Self::Hazard => 5,
            Self::Cusp => 6,
        }
    }
}

/// A declared scalar is either finite or explicitly missing with a reason.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModuleDatum {
    Present(FiniteF64),
    Missing(MissingnessReason),
}

impl ModuleDatum {
    #[must_use]
    pub const fn value(&self) -> Option<FiniteF64> {
        match self {
            Self::Present(value) => Some(*value),
            Self::Missing(_) => None,
        }
    }

    #[must_use]
    pub const fn missingness_reason(&self) -> Option<MissingnessReason> {
        match self {
            Self::Present(_) => None,
            Self::Missing(reason) => Some(*reason),
        }
    }
}

/// Runtime state of a module output, independent of scientific eligibility.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ModuleAvailability {
    Available,
    Degraded(MissingnessReason),
    Unavailable(MissingnessReason),
    Abstained(MissingnessReason),
}

impl ModuleAvailability {
    pub(crate) const fn identity_tag(self) -> u8 {
        match self {
            Self::Available => 1,
            Self::Degraded(_) => 2,
            Self::Unavailable(_) => 3,
            Self::Abstained(_) => 4,
        }
    }

    pub(crate) const fn reason(self) -> Option<MissingnessReason> {
        match self {
            Self::Available => None,
            Self::Degraded(reason) | Self::Unavailable(reason) | Self::Abstained(reason) => {
                Some(reason)
            }
        }
    }

    pub(crate) const fn permits_absent_output(self) -> bool {
        matches!(self, Self::Unavailable(_) | Self::Abstained(_))
    }
}

/// Untrusted boundary for one independently fitted module prediction.
///
/// `trained_through` is the inclusive training-data cutoff, `as_known_at` is
/// the earliest timestamp at which the complete output was knowable, and
/// `origin` is the prediction timestamp. Validation requires
/// `trained_through <= as_known_at <= origin` and a cutoff strictly before the
/// prediction origin.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModuleOutputInput {
    pub module_kind: ModuleKind,
    pub model_id: String,
    pub model_package_hash: [u8; 32],
    pub entity: FeatureEntity,
    pub origin: UnixNanos,
    pub as_known_at: UnixNanos,
    pub trained_through: UnixNanos,
    pub outer_fold_hash: [u8; 32],
    pub values: BTreeMap<FeatureId, ModuleDatum>,
    pub availability: ModuleAvailability,
    pub quality: QualityScore,
    pub source_evidence_hash: [u8; 32],
}

/// Module output fields whose package identity will be derived from verification.
///
/// Timing has the same ordering and strict pre-origin training cutoff as
/// [`ModuleOutputInput`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerifiedModuleOutputInput {
    pub module_kind: ModuleKind,
    pub entity: FeatureEntity,
    pub origin: UnixNanos,
    pub as_known_at: UnixNanos,
    pub trained_through: UnixNanos,
    pub values: BTreeMap<FeatureId, ModuleDatum>,
    pub availability: ModuleAvailability,
    pub quality: QualityScore,
    pub source_evidence_hash: [u8; 32],
}

/// Validated point-in-time output from one independently fitted module.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModuleOutput {
    module_kind: ModuleKind,
    model_id: String,
    model_package_hash: [u8; 32],
    entity: FeatureEntity,
    origin: UnixNanos,
    as_known_at: UnixNanos,
    trained_through: UnixNanos,
    outer_fold_hash: [u8; 32],
    values: BTreeMap<FeatureId, ModuleDatum>,
    availability: ModuleAvailability,
    quality: QualityScore,
    source_evidence_hash: [u8; 32],
    output_hash: [u8; 32],
}

impl ModuleOutput {
    /// Validates a caller-declared package identity at an ingestion boundary.
    pub fn try_new(input: ModuleOutputInput) -> Result<Self, EnsembleError> {
        validate_input(&input)?;
        let output_hash = calculate_output_hash(&input)?;
        Ok(Self {
            module_kind: input.module_kind,
            model_id: input.model_id,
            model_package_hash: input.model_package_hash,
            entity: input.entity,
            origin: input.origin,
            as_known_at: input.as_known_at,
            trained_through: input.trained_through,
            outer_fold_hash: input.outer_fold_hash,
            values: input.values,
            availability: input.availability,
            quality: input.quality,
            source_evidence_hash: input.source_evidence_hash,
            output_hash,
        })
    }

    /// Derives model identity from a signature- and artifact-verified package.
    pub fn try_from_verified_package(
        input: VerifiedModuleOutputInput,
        package: &VerifiedPackage,
        outer_fold: &OuterFold,
    ) -> Result<Self, EnsembleError> {
        let manifest = &package.package().manifest;
        if input.trained_through.value() != manifest.training_period.end_ns
            || manifest.outer_test_period.start_ns != outer_fold.test().start_ns()
            || manifest.outer_test_period.end_ns != outer_fold.test().end_ns()
            || input.origin.value() < outer_fold.test().start_ns()
            || input.origin.value() >= outer_fold.test().end_ns()
        {
            return Err(EnsembleError::InvalidModelPackage);
        }
        Self::try_new(ModuleOutputInput {
            module_kind: input.module_kind,
            model_id: package.model_id().to_owned(),
            model_package_hash: package.package_blake3(),
            entity: input.entity,
            origin: input.origin,
            as_known_at: input.as_known_at,
            trained_through: input.trained_through,
            outer_fold_hash: outer_fold.fold_hash(),
            values: input.values,
            availability: input.availability,
            quality: input.quality,
            source_evidence_hash: input.source_evidence_hash,
        })
    }

    #[must_use]
    pub const fn module_kind(&self) -> ModuleKind {
        self.module_kind
    }

    #[must_use]
    pub fn model_id(&self) -> &str {
        &self.model_id
    }

    #[must_use]
    pub const fn model_package_hash(&self) -> [u8; 32] {
        self.model_package_hash
    }

    #[must_use]
    pub const fn entity(&self) -> &FeatureEntity {
        &self.entity
    }

    #[must_use]
    pub const fn origin(&self) -> UnixNanos {
        self.origin
    }

    #[must_use]
    pub const fn as_known_at(&self) -> UnixNanos {
        self.as_known_at
    }

    #[must_use]
    pub const fn trained_through(&self) -> UnixNanos {
        self.trained_through
    }

    #[must_use]
    pub const fn outer_fold_hash(&self) -> [u8; 32] {
        self.outer_fold_hash
    }

    #[must_use]
    pub const fn values(&self) -> &BTreeMap<FeatureId, ModuleDatum> {
        &self.values
    }

    #[must_use]
    pub const fn availability(&self) -> ModuleAvailability {
        self.availability
    }

    #[must_use]
    pub const fn quality(&self) -> QualityScore {
        self.quality
    }

    #[must_use]
    pub const fn source_evidence_hash(&self) -> [u8; 32] {
        self.source_evidence_hash
    }

    #[must_use]
    pub const fn output_hash(&self) -> [u8; 32] {
        self.output_hash
    }
}

fn validate_input(input: &ModuleOutputInput) -> Result<(), EnsembleError> {
    if !valid_identifier(&input.model_id) {
        return Err(EnsembleError::InvalidModelPackage);
    }
    require_nonzero_digest(&input.model_package_hash)?;
    require_nonzero_digest(&input.outer_fold_hash)?;
    require_nonzero_digest(&input.source_evidence_hash)?;
    if input.origin.value() <= 0
        || input.as_known_at.value() <= 0
        || input.trained_through.value() <= 0
        || input.trained_through >= input.origin
        || input.as_known_at < input.trained_through
    {
        return Err(EnsembleError::InvalidModuleOutput);
    }
    if input.as_known_at > input.origin {
        return Err(EnsembleError::FutureKnownOutput);
    }
    if input.values.is_empty() || input.values.len() > MAXIMUM_VALUES_PER_MODULE {
        return Err(EnsembleError::ColumnCapacity);
    }
    validate_availability(input.availability, input.values.values())
}

fn validate_availability<'a>(
    availability: ModuleAvailability,
    values: impl Iterator<Item = &'a ModuleDatum>,
) -> Result<(), EnsembleError> {
    let mut present = 0_usize;
    let mut missing = 0_usize;
    for value in values {
        match value {
            ModuleDatum::Present(_) => present += 1,
            ModuleDatum::Missing(_) => missing += 1,
        }
    }
    let valid = match availability {
        ModuleAvailability::Available => present > 0 && missing == 0,
        ModuleAvailability::Degraded(_) => present > 0,
        ModuleAvailability::Unavailable(_) | ModuleAvailability::Abstained(_) => {
            present == 0 && missing > 0
        }
    };
    if valid {
        Ok(())
    } else {
        Err(EnsembleError::AvailabilityContradiction)
    }
}

fn calculate_output_hash(input: &ModuleOutputInput) -> Result<[u8; 32], EnsembleError> {
    let mut hasher = blake3::Hasher::new();
    hasher.update(MODULE_OUTPUT_DOMAIN);
    hasher.update(&[input.module_kind.identity_tag()]);
    hash_string(&mut hasher, &input.model_id);
    hasher.update(&input.model_package_hash);
    hash_entity(&mut hasher, &input.entity)?;
    hash_i64(&mut hasher, input.origin.value());
    hash_i64(&mut hasher, input.as_known_at.value());
    hash_i64(&mut hasher, input.trained_through.value());
    hasher.update(&input.outer_fold_hash);
    hash_u64(
        &mut hasher,
        u64::try_from(input.values.len()).unwrap_or(u64::MAX),
    );
    for (identifier, value) in &input.values {
        hash_string(&mut hasher, identifier.as_str());
        hash_module_datum(&mut hasher, value);
    }
    hash_availability(&mut hasher, input.availability);
    hash_u64(&mut hasher, u64::from(input.quality.millionths()));
    hasher.update(&input.source_evidence_hash);
    Ok(*hasher.finalize().as_bytes())
}

pub(crate) fn hash_entity(
    hasher: &mut blake3::Hasher,
    entity: &FeatureEntity,
) -> Result<(), EnsembleError> {
    let bytes = serde_json::to_vec(entity).map_err(|_| EnsembleError::EntitySerialization)?;
    hash_u64(hasher, u64::try_from(bytes.len()).unwrap_or(u64::MAX));
    hasher.update(&bytes);
    Ok(())
}

pub(crate) fn entity_key(entity: &FeatureEntity) -> Result<Vec<u8>, EnsembleError> {
    serde_json::to_vec(entity).map_err(|_| EnsembleError::EntitySerialization)
}

pub(crate) fn hash_module_datum(hasher: &mut blake3::Hasher, datum: &ModuleDatum) {
    match datum {
        ModuleDatum::Present(value) => {
            hasher.update(&[1]);
            hasher.update(&value.value().to_bits().to_le_bytes());
        }
        ModuleDatum::Missing(reason) => {
            hasher.update(&[2, missingness_identity_tag(*reason)]);
        }
    }
}

pub(crate) fn hash_availability(hasher: &mut blake3::Hasher, availability: ModuleAvailability) {
    hasher.update(&[availability.identity_tag()]);
    if let Some(reason) = availability.reason() {
        hasher.update(&[missingness_identity_tag(reason)]);
    }
}

pub(crate) const fn missingness_identity_tag(reason: MissingnessReason) -> u8 {
    match reason {
        MissingnessReason::NotListed => 1,
        MissingnessReason::SourceNotSupported => 2,
        MissingnessReason::SourceDisconnected => 3,
        MissingnessReason::SequenceGap => 4,
        MissingnessReason::Stale => 5,
        MissingnessReason::InsufficientHistory => 6,
        MissingnessReason::WindowNotFinal => 7,
        MissingnessReason::BelowLiquidityThreshold => 8,
        MissingnessReason::VendorRevisionPending => 9,
        MissingnessReason::ModelNotApplicable => 10,
        MissingnessReason::PrivacyOrLicenseRestriction => 11,
        MissingnessReason::Unknown => 12,
    }
}
