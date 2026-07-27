//! Durable, versioned raw market-data write-ahead log.

pub mod frame;
pub mod recovery;
pub mod segment;

pub use recovery::{RecoveryError, RecoveryReport};
pub use segment::{Segment, SegmentError};
