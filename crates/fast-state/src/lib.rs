//! Deterministic fast-state cadence and immutable point-in-time snapshots.

mod features;
mod scheduler;
mod snapshot;
mod trigger_state;

pub use features::{
    cancellation_burst, completeness_weighted_liquidation_pressure, depth_disappearance,
    dispersion_acceleration,
};
pub use scheduler::{EventTimeliness, FastStateScheduler, TickEvent, TickKind};
pub use snapshot::{
    CoalescingSnapshotSlot, FastStateSnapshot, FastStateSnapshotInput, MaskedFeatureVector,
    PublishedFastStateSnapshot, SnapshotHealth,
};
pub use trigger_state::{
    TriggerError, TriggerHealth, TriggerMissingness, TriggerState, TriggerStateBuilder,
    TriggerStateTarget,
};

use feature_registry::RegistryError;
use thiserror::Error;

/// Fail-closed scheduler and snapshot errors.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum FastStateError {
    #[error("invalid fast-state scheduler configuration")]
    InvalidScheduler,
    #[error("fast-state clock regressed from {current_ns} to {attempted_ns}")]
    ClockRegression { current_ns: i64, attempted_ns: i64 },
    #[error("fast-state tick backlog capacity reached")]
    TickBacklogCapacity,
    #[error("fast-state counter overflow")]
    CounterOverflow,
    #[error("fast-state scheduler is cancelled")]
    Cancelled,
    #[error("event time must be nonnegative")]
    InvalidEventTime,
    #[error("event time is ahead of the scheduler")]
    EventAheadOfScheduler,
    #[error("fast-state snapshot has invalid point-in-time ordering")]
    InvalidSnapshotTime,
    #[error("fast-state publication requires a one-second publication tick")]
    PublicationRequiresPublishedTick,
    #[error("fast-state publication tick and snapshot time differ")]
    PublicationTimeMismatch,
    #[error("fast-state snapshot feature vector is empty or exceeds its bound")]
    InvalidFeatureCount,
    #[error("fast-state snapshot contains an invalid feature observation: {0}")]
    InvalidFeatureObservation(#[from] RegistryError),
    #[error("fast-state snapshot contains a duplicate feature observation")]
    DuplicateFeatureObservation,
    #[error("fast-state snapshot mixes entity identities")]
    MixedSnapshotEntity,
    #[error("fast-state snapshot contains a non-float model input")]
    UnsupportedFeatureValue,
    #[error("fast-state snapshot evidence could not be encoded")]
    EvidenceEncoding,
    #[error("fast-state UI snapshot regressed")]
    SnapshotRegression,
    #[error("fast-state same-event correction does not monotonically advance revisions")]
    SnapshotCorrectionMismatch,
    #[error("fast-state UI slot received a different entity identity")]
    SnapshotEntityMismatch,
}
