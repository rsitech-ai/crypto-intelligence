//! Pure source-quality state machine with consecutive event lineage.

use std::num::NonZeroU64;

use domain::{SourceId, UnixNanos};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Whether downstream consumers may trust one source's current observations.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceHealthState {
    Healthy,
    Degraded,
    Unhealthy,
    Quarantined,
    Recovering,
}

impl SourceHealthState {
    pub const ALL: [Self; 5] = [
        Self::Healthy,
        Self::Degraded,
        Self::Unhealthy,
        Self::Quarantined,
        Self::Recovering,
    ];

    pub fn transition_to(self, to: Self) -> Result<Self, QualityError> {
        if valid_transition(self, to) {
            Ok(to)
        } else {
            Err(QualityError::InvalidTransition { from: self, to })
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::Degraded => "degraded",
            Self::Unhealthy => "unhealthy",
            Self::Quarantined => "quarantined",
            Self::Recovering => "recovering",
        }
    }
}

/// Bounded, typed evidence that explains a source-quality transition.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QualityCause {
    RecoveryVerified,
    FreshnessRestored,
    SourceStale,
    CapacityPressure,
    SequenceIntegrityFailed,
    ChecksumIntegrityFailed,
    SchemaValidityFailed,
    NormalizationFailed,
    RecoveryStarted,
    RetryBudgetExhausted,
    PolicyQuarantine,
    PolicyReleased,
}

impl QualityCause {
    pub const ALL: [Self; 12] = [
        Self::RecoveryVerified,
        Self::FreshnessRestored,
        Self::SourceStale,
        Self::CapacityPressure,
        Self::SequenceIntegrityFailed,
        Self::ChecksumIntegrityFailed,
        Self::SchemaValidityFailed,
        Self::NormalizationFailed,
        Self::RecoveryStarted,
        Self::RetryBudgetExhausted,
        Self::PolicyQuarantine,
        Self::PolicyReleased,
    ];

    pub fn is_compatible(self, from: SourceHealthState, to: SourceHealthState) -> bool {
        valid_transition(from, to) && valid_cause(from, to, self)
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RecoveryVerified => "recovery_verified",
            Self::FreshnessRestored => "freshness_restored",
            Self::SourceStale => "source_stale",
            Self::CapacityPressure => "capacity_pressure",
            Self::SequenceIntegrityFailed => "sequence_integrity_failed",
            Self::ChecksumIntegrityFailed => "checksum_integrity_failed",
            Self::SchemaValidityFailed => "schema_validity_failed",
            Self::NormalizationFailed => "normalization_failed",
            Self::RecoveryStarted => "recovery_started",
            Self::RetryBudgetExhausted => "retry_budget_exhausted",
            Self::PolicyQuarantine => "policy_quarantine",
            Self::PolicyReleased => "policy_released",
        }
    }
}

/// One consecutive quality observation for a specific source connection.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct QualityEvent {
    source: SourceId,
    connection_epoch: NonZeroU64,
    sequence: NonZeroU64,
    observed_at: UnixNanos,
    from: SourceHealthState,
    to: SourceHealthState,
    cause: QualityCause,
}

impl QualityEvent {
    pub const fn source(&self) -> &SourceId {
        &self.source
    }

    pub const fn connection_epoch(&self) -> NonZeroU64 {
        self.connection_epoch
    }

    pub const fn sequence(&self) -> NonZeroU64 {
        self.sequence
    }

    pub const fn observed_at(&self) -> UnixNanos {
        self.observed_at
    }

    pub const fn from(&self) -> SourceHealthState {
        self.from
    }

    pub const fn to(&self) -> SourceHealthState {
        self.to
    }

    pub const fn cause(&self) -> QualityCause {
        self.cause
    }
}

/// Authoritative quality state for one source and connection epoch.
pub struct SourceHealthTracker {
    source: SourceId,
    connection_epoch: NonZeroU64,
    state: SourceHealthState,
    next_sequence: NonZeroU64,
    last_observed_at: Option<UnixNanos>,
}

impl SourceHealthTracker {
    /// A newly created or renewed source is untrusted until recovery is proven.
    pub fn new(source: SourceId, connection_epoch: NonZeroU64) -> Self {
        Self {
            source,
            connection_epoch,
            state: SourceHealthState::Recovering,
            next_sequence: NonZeroU64::MIN,
            last_observed_at: None,
        }
    }

    pub const fn source(&self) -> &SourceId {
        &self.source
    }

    pub const fn connection_epoch(&self) -> NonZeroU64 {
        self.connection_epoch
    }

    pub const fn state(&self) -> SourceHealthState {
        self.state
    }

    pub const fn last_observed_at(&self) -> Option<UnixNanos> {
        self.last_observed_at
    }

    pub fn ensure_sequence_capacity(&self, additional_events: usize) -> Result<(), QualityError> {
        let additional_events =
            u64::try_from(additional_events).map_err(|_| QualityError::SequenceExhausted)?;
        self.next_sequence
            .get()
            .checked_add(additional_events)
            .ok_or(QualityError::SequenceExhausted)?;
        Ok(())
    }

    pub fn prepare(
        &self,
        to: SourceHealthState,
        cause: QualityCause,
        observed_at: UnixNanos,
    ) -> Result<PreparedQualityTransition, QualityError> {
        self.state.transition_to(to)?;
        if !valid_cause(self.state, to, cause) {
            return Err(QualityError::IncompatibleCause);
        }
        if observed_at.value() <= 0 {
            return Err(QualityError::InvalidObservationTime);
        }
        if self
            .last_observed_at
            .is_some_and(|previous| observed_at <= previous)
        {
            return Err(QualityError::NonMonotonicObservationTime);
        }
        let next_sequence = self
            .next_sequence
            .get()
            .checked_add(1)
            .and_then(NonZeroU64::new)
            .ok_or(QualityError::SequenceExhausted)?;
        Ok(PreparedQualityTransition {
            event: QualityEvent {
                source: self.source.clone(),
                connection_epoch: self.connection_epoch,
                sequence: self.next_sequence,
                observed_at,
                from: self.state,
                to,
                cause,
            },
            next_sequence,
        })
    }

    pub fn commit(&mut self, prepared: PreparedQualityTransition) -> Result<(), QualityError> {
        if prepared.event.source != self.source
            || prepared.event.connection_epoch != self.connection_epoch
            || prepared.event.from != self.state
            || prepared.event.sequence != self.next_sequence
            || self
                .last_observed_at
                .is_some_and(|previous| prepared.event.observed_at <= previous)
        {
            return Err(QualityError::StaleTransition);
        }
        self.state = prepared.event.to;
        self.next_sequence = prepared.next_sequence;
        self.last_observed_at = Some(prepared.event.observed_at);
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreparedQualityTransition {
    event: QualityEvent,
    next_sequence: NonZeroU64,
}

impl PreparedQualityTransition {
    pub const fn event(&self) -> &QualityEvent {
        &self.event
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum QualityError {
    #[error("invalid source-quality transition: {from:?} -> {to:?}")]
    InvalidTransition {
        from: SourceHealthState,
        to: SourceHealthState,
    },
    #[error("quality cause is incompatible with the requested transition")]
    IncompatibleCause,
    #[error("quality observation time must be positive")]
    InvalidObservationTime,
    #[error("quality observation time did not advance")]
    NonMonotonicObservationTime,
    #[error("prepared quality transition is stale or belongs to another tracker")]
    StaleTransition,
    #[error("quality event sequence is exhausted")]
    SequenceExhausted,
}

fn valid_transition(from: SourceHealthState, to: SourceHealthState) -> bool {
    use SourceHealthState::{Degraded, Healthy, Quarantined, Recovering, Unhealthy};
    matches!(
        (from, to),
        (Recovering, Healthy | Degraded | Unhealthy | Quarantined)
            | (Healthy, Degraded | Unhealthy | Quarantined)
            | (Degraded, Healthy | Unhealthy | Recovering | Quarantined)
            | (Unhealthy, Recovering | Quarantined)
            | (Quarantined, Recovering)
    )
}

fn valid_cause(from: SourceHealthState, to: SourceHealthState, cause: QualityCause) -> bool {
    use QualityCause::{
        CapacityPressure, ChecksumIntegrityFailed, FreshnessRestored, NormalizationFailed,
        PolicyQuarantine, PolicyReleased, RecoveryStarted, RecoveryVerified, RetryBudgetExhausted,
        SchemaValidityFailed, SequenceIntegrityFailed, SourceStale,
    };
    use SourceHealthState::{Degraded, Healthy, Quarantined, Recovering, Unhealthy};

    match cause {
        RecoveryVerified => from == Recovering && to == Healthy,
        FreshnessRestored => from == Degraded && to == Healthy,
        SourceStale => matches!((from, to), (Healthy, Degraded) | (Recovering, Degraded)),
        CapacityPressure => {
            matches!(
                (from, to),
                (Healthy, Degraded) | (Recovering, Degraded) | (_, Unhealthy)
            )
        }
        SequenceIntegrityFailed | ChecksumIntegrityFailed => to == Unhealthy,
        SchemaValidityFailed | NormalizationFailed => to == Unhealthy,
        RecoveryStarted => matches!((from, to), (Degraded, Recovering) | (Unhealthy, Recovering)),
        RetryBudgetExhausted => {
            matches!((from, to), (Degraded | Unhealthy | Recovering, Quarantined))
        }
        PolicyQuarantine => to == Quarantined,
        PolicyReleased => from == Quarantined && to == Recovering,
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU64;

    use domain::{SourceId, SourceKind};

    use super::{QualityError, SourceHealthTracker};

    #[test]
    fn sequence_capacity_preflight_rejects_a_batch_before_any_transition() {
        let source =
            SourceId::new(SourceKind::Exchange, "sequence-boundary", 1).expect("source identity");
        let mut tracker = SourceHealthTracker::new(source, NonZeroU64::MIN);
        tracker.next_sequence = NonZeroU64::new(u64::MAX - 1).expect("bounded sequence");

        assert_eq!(
            tracker.ensure_sequence_capacity(2),
            Err(QualityError::SequenceExhausted)
        );
        assert_eq!(tracker.next_sequence.get(), u64::MAX - 1);
    }
}
