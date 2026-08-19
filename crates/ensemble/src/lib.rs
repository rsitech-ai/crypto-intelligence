//! Leakage-safe inputs for the calibrated model ensemble.

pub mod constraints;
pub mod module_output;
pub mod oof_matrix;

pub use constraints::{CuspEligibilityReceipt, MatrixConstraints};
pub use module_output::{
    ModuleAvailability, ModuleDatum, ModuleKind, ModuleOutput, ModuleOutputInput,
    VerifiedModuleOutputInput,
};
pub use oof_matrix::{
    EnsembleSchema, MatrixColumn, MatrixDatum, MatrixLimits, MatrixModuleState, MatrixRow,
    OutOfFoldMatrix, RowModule, TrainingRowInput,
};

use thiserror::Error;

/// Fail-closed errors at the ensemble training boundary.
#[derive(Clone, Debug, Error, PartialEq)]
pub enum EnsembleError {
    #[error("module output timing or lineage is invalid")]
    InvalidModuleOutput,
    #[error("module output contains a zero evidence or package digest")]
    ZeroDigest,
    #[error("module output uses information after its prediction origin")]
    FutureKnownOutput,
    #[error("module availability contradicts its reported value states")]
    AvailabilityContradiction,
    #[error("out-of-fold matrix has no rows")]
    EmptyRows,
    #[error("out-of-fold matrix has no module columns")]
    EmptyColumns,
    #[error("ensemble schema version is unsupported")]
    UnsupportedSchema,
    #[error("ensemble schema contains a duplicate module feature column")]
    DuplicateColumn,
    #[error("out-of-fold row is invalid")]
    InvalidRow,
    #[error("out-of-fold row identity is duplicated")]
    DuplicateRow,
    #[error("one row contains the same module more than once")]
    DuplicateModule,
    #[error("row module coverage does not exactly match the declared schema")]
    ModuleCoverageMismatch,
    #[error("an explicitly absent module has an invalid availability state")]
    InvalidMissingModule,
    #[error("module output belongs to a different entity")]
    EntityMismatch,
    #[error("module output belongs to a different prediction origin")]
    OriginMismatch,
    #[error("module output belongs to a different outer fold")]
    FoldMismatch,
    #[error("module prediction was trained into the outer test fold")]
    InFoldPrediction,
    #[error("module value schema changed between out-of-fold rows")]
    SchemaMismatch,
    #[error("one outer fold contains inconsistent model package identity for a module")]
    ModulePackageMismatch,
    #[error("eligible Cusp gate candidate does not match the reported Cusp model package")]
    CuspCandidateMismatch,
    #[error("module output model package identity is invalid")]
    InvalidModelPackage,
    #[error("canonical entity identity could not be serialized")]
    EntitySerialization,
    #[error("matrix limits are zero or exceed the global ceilings")]
    InvalidLimits,
    #[error("out-of-fold row capacity exceeded")]
    RowCapacity,
    #[error("out-of-fold module or column capacity exceeded")]
    ColumnCapacity,
    #[error("out-of-fold dense cell capacity exceeded")]
    CellCapacity,
}

pub(crate) fn valid_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b':' | b'/')
        })
}

pub(crate) fn require_nonzero_digest(digest: &[u8; 32]) -> Result<(), EnsembleError> {
    if *digest == [0; 32] {
        Err(EnsembleError::ZeroDigest)
    } else {
        Ok(())
    }
}

pub(crate) fn hash_u64(hasher: &mut blake3::Hasher, value: u64) {
    hasher.update(&value.to_le_bytes());
}

pub(crate) fn hash_i64(hasher: &mut blake3::Hasher, value: i64) {
    hasher.update(&value.to_le_bytes());
}

pub(crate) fn hash_string(hasher: &mut blake3::Hasher, value: &str) {
    hash_u64(hasher, u64::try_from(value.len()).unwrap_or(u64::MAX));
    hasher.update(value.as_bytes());
}
