//! Signed, immutable model packages and an auditable fail-closed lifecycle.

mod package;
mod state;
mod verification;

pub use package::{
    CalibrationDescriptor, MetricSummary, ModelPackageManifest, ModelPackageManifestInput,
    PackageBuildError, PackagePeriod, PackageSigner, QualityRequirements, RuntimeRequirements,
    SignedModelPackage,
};
pub use state::{
    IndependentReview, ModelRecord, ModelRegistry, ModelState, PromotionError, ShadowEvidence,
    TransitionRecord,
};
pub use verification::{
    CompatibilityRequest, CompatiblePackage, PackageVerificationError, TrustedVerifyingKey,
    VerifiedPackage, verify_compatibility, verify_package,
};
