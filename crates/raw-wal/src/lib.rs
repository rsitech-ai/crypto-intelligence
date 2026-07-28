//! Durable, versioned raw market-data write-ahead log.

pub mod frame;
pub mod prologue;
pub mod recovery;
pub mod segment;

pub use recovery::{
    CorruptionKind, RecoveredRecord, RecoveredRecordOwned, RecoveryError, RecoveryReport,
    RecoverySummary,
};
pub use segment::{Segment, SegmentError};
