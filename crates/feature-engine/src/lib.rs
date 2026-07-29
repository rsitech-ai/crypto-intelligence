//! Deterministic point-in-time watermark, window, and timer primitives.

pub mod clock;
pub mod features;
pub mod lineage;
pub mod materialize;
pub mod watermark;
pub mod window;

pub use clock::{ClockBasis, ClockError, LogicalClock, RecordedClock, RecordedTimestamp, TimerId};
pub use lineage::{FeatureLineage, FeatureLineageInput, LineageError};
pub use materialize::{
    MaterializationError, MaterializationMode, MaterializationReport, MaterializedFeatureRow,
    Materializer, PointInTimeFeatureSet, PointInTimeTrainingPlan,
};
pub use watermark::{
    CorrectionDecision, Finalization, FinalizationDecision, PartitionConfig, PartitionId,
    WatermarkError, WatermarkKey, WatermarkTracker, WatermarkUpdate,
};
pub use window::{
    CountWindow, EmissionAction, EventCountState, HalfLifeEwmaState, ThresholdKind,
    ThresholdWindow, ThresholdWindowState, TimeWindow, TimeWindowSpec, WindowError,
    WindowLifecycle,
};
