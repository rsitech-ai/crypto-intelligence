//! Local-only structured observability primitives.

pub mod fields;
pub mod local_service;
pub mod metrics;
pub mod redaction;
pub mod tracing_local;

pub use fields::FieldName;
pub use local_service::{LocalJsonLog, LogRotationPolicy, ObservabilityHandle};
pub use metrics::{
    Component, DiagnosticsSnapshot, MetricKey, MetricKind, MetricName, MetricSeries,
    MetricSnapshotValue, MetricUnit, Metrics, Outcome, SloObservation, Venue,
};
pub use redaction::{REDACTED, RawPayload, Redactor, SecretValue};
use thiserror::Error;
pub use tracing_local::{
    LocalLogLevel, LocalTracing, TestObservability, init_local_tracing, init_test_observability,
};

#[derive(Debug, Error)]
pub enum ObservabilityError {
    #[error("metric operation does not match the metric's declared kind")]
    MetricType,
    #[error("metric observation must be finite, nonnegative, and preserve a finite sum")]
    InvalidObservation,
    #[error("diagnostics snapshot time must be positive")]
    InvalidTimestamp,
    #[error("bounded metric series capacity is exhausted")]
    SeriesCapacity,
    #[error("local tracing subscriber installation failed")]
    SubscriberInstall,
    #[error("local tracing could not write {count} structured events")]
    LogWriteFailures { count: u64 },
    #[error("observability state is poisoned")]
    Poisoned,
    #[error("structured log line exceeds the local bound")]
    LogLineTooLarge,
    #[error("local log rotation policy is outside the supported safe bounds")]
    InvalidRotationPolicy,
    #[error("local log segment metadata violates the private-file contract")]
    UnsafeLogFile,
    #[error("local log segment size overflowed")]
    LogSizeOverflow,
    #[error("structured log serialization failed: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("local log I/O failed: {0}")]
    Io(#[from] std::io::Error),
}
