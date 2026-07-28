//! Deterministic, evidence-aware local capacity planning.

pub mod admission;
pub mod estimate;
pub mod profile;

pub use admission::{
    AdmissionDecision, AdmissionReason, CapacityQualityImpact, DowngradeProposal,
    EvidenceRequirement, admit,
};
pub use estimate::CapacityEstimate;
pub use profile::{
    CapacityEvidence, CapacityInput, CapacityPolicy, EvidenceLevel, HardwareProfile, InputField,
    WorkloadProfile,
};

use thiserror::Error;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
pub enum CapacityError {
    #[error("capacity input field {field:?} violates its versioned bounds")]
    InvalidInput { field: InputField },
    #[error("capacity arithmetic overflowed")]
    Overflow,
}
