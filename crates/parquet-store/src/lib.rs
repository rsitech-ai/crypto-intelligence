//! Immutable, manifest-published Parquet datasets for normalized local data.
//!
//! [`DatasetWriter`] writes Arrow batches into durable active fragments and
//! publishes a sealed partition only after compacting and syncing the final
//! Parquet file. The manifest is the publication boundary: files without a
//! valid manifest are never treated as sealed history.

mod layout;
mod manifest;
mod writer;

use std::io;

pub use layout::{
    BatchId, CorrectionLineage, DatasetMetadata, DatasetMetadataInput, PartitionKey,
    PartitionKeyInput, SourceCoverage,
};
pub use manifest::{Manifest, ManifestFile, SourceFragmentLineage, VerifiedPartition};
#[cfg(feature = "test-support")]
pub use writer::FaultPoint;
pub use writer::{
    DatasetPolicy, DatasetPolicyInput, DatasetReader, DatasetWriter, Fragment, WriteReceipt,
};

/// Failure returned by the bounded local Parquet store.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("partition key is invalid")]
    InvalidPartitionKey,
    #[error("dataset metadata is invalid")]
    InvalidDatasetMetadata,
    #[error("dataset policy is invalid")]
    InvalidPolicy,
    #[error("dataset root is not a private owner-controlled directory")]
    UnsafeRoot,
    #[error("dataset path contains an unsafe filesystem object")]
    UnsafePath,
    #[error("another dataset writer already owns this root")]
    AlreadyOpen,
    #[error("record batch is empty or exceeds a configured bound")]
    InvalidBatch,
    #[error("batch identity is invalid")]
    InvalidBatchIdentity,
    #[error("batch identity was already committed with different content")]
    BatchIdentityConflict,
    #[error("record batch schema does not match the partition schema")]
    SchemaMismatch,
    #[error("record batch requires canonical effective, exchange, and receive time columns")]
    InvalidEventTimeColumn,
    #[error("event time does not belong to the selected UTC partition hour")]
    EventTimeOutsidePartition,
    #[error("record batch dimensions do not match the selected partition")]
    PartitionDimensionMismatch,
    #[error("dataset correction lineage is missing, inconsistent, or unverifiable")]
    InvalidCorrectionLineage,
    #[error("partition is already sealed and immutable")]
    PartitionSealed,
    #[error("partition has no active fragments to seal")]
    PartitionEmpty,
    #[error("active fragment inventory exceeds its bound")]
    FragmentCapacityExceeded,
    #[error("sealed Parquet inventory exceeds its bound")]
    FileCapacityExceeded,
    #[error("manifest or Parquet content failed integrity verification")]
    Integrity,
    #[error("bounded metadata or manifest input is too large")]
    CapacityExceeded,
    #[error("integer conversion or arithmetic overflow")]
    IntegerRange,
    #[cfg(feature = "test-support")]
    #[error("test-support fault was injected after a durable publication boundary")]
    InjectedFault,
    #[error("Parquet operation failed")]
    Parquet(#[from] parquet::errors::ParquetError),
    #[error("Arrow operation failed")]
    Arrow(#[from] arrow::error::ArrowError),
    #[error("JSON operation failed")]
    Json(#[from] serde_json::Error),
    #[error("filesystem operation failed")]
    Io(#[from] io::Error),
}
