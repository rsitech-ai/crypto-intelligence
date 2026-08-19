//! Bounded collector admission, retry, overflow, quality, and shutdown policy.

pub mod supervisor;

pub use quality::{QualityCause, QualityEvent, SourceHealthState, SourceHealthTracker};
pub use supervisor::{
    AdmissionError, AdmissionLimits, CollectorSupervisor, CoverageTier, OfferOutcome,
    OperationalQualityEvent, OperationalQualityReceipt, OperationalQualitySampleInput,
    OverflowAction, OverflowReport, QueueClass, QueueMetrics, QueueTracker, RetryDecision,
    RetryPolicy, ShutdownActions, ShutdownError, ShutdownPhase, ShutdownReport, SourcePolicy,
    StoragePressureReceipt, SupervisorHarness, execute_shutdown, overflow_action,
};
