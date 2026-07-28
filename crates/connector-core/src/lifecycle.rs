//! Pure connector lifecycle state machine and bounded lifecycle events.

use std::num::NonZeroU64;

use domain::{SourceId, UnixNanos};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Normative collector lifecycle from production specification 29.2.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectorState {
    Stopped,
    Connecting,
    AuthenticatingOrSubscribing,
    Synchronizing,
    Healthy,
    Degraded,
    BackingOff,
    Recovering,
    Quarantined,
}

impl ConnectorState {
    pub const ALL: [Self; 9] = [
        Self::Stopped,
        Self::Connecting,
        Self::AuthenticatingOrSubscribing,
        Self::Synchronizing,
        Self::Healthy,
        Self::Degraded,
        Self::BackingOff,
        Self::Recovering,
        Self::Quarantined,
    ];

    pub fn transition_to(self, next: Self) -> Result<Self, LifecycleError> {
        if valid_transition(self, next) {
            Ok(next)
        } else {
            Err(LifecycleError::InvalidTransition {
                from: self,
                to: next,
            })
        }
    }
}

fn valid_transition(from: ConnectorState, to: ConnectorState) -> bool {
    use ConnectorState::{
        AuthenticatingOrSubscribing as Auth, BackingOff, Connecting, Degraded, Healthy,
        Quarantined, Recovering, Stopped, Synchronizing,
    };
    if from == to || from == Stopped && to != Connecting {
        return false;
    }
    if to == Stopped {
        return from != Stopped;
    }
    if to == Quarantined {
        return from != Stopped && from != Quarantined;
    }
    matches!(
        (from, to),
        (Stopped, Connecting)
            | (Connecting, Auth | BackingOff)
            | (Auth, Synchronizing | BackingOff)
            | (Synchronizing, Healthy | Degraded | BackingOff | Recovering)
            | (Healthy, Degraded | Recovering | BackingOff)
            | (Degraded, Healthy | Recovering | BackingOff)
            | (BackingOff, Connecting)
            | (Recovering, Synchronizing | BackingOff)
            | (Quarantined, Recovering)
    )
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum LifecycleError {
    #[error("invalid connector lifecycle transition: {from:?} -> {to:?}")]
    InvalidTransition {
        from: ConnectorState,
        to: ConnectorState,
    },
    #[error("lifecycle cause is incompatible with the requested transition")]
    IncompatibleCause,
    #[error("lifecycle sequence is exhausted")]
    SequenceExhausted,
}

/// Typed, bounded reasons attached to lifecycle changes.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleCause {
    Startup,
    SubscriptionAccepted,
    ResynchronizationStarted,
    SnapshotApplied,
    SourceRecovered,
    SourceStale,
    SequenceGap,
    CapacityRejected,
    RecoverableDisconnect,
    BackoffElapsed,
    SupervisorCommand,
    SchemaViolation,
    Shutdown,
}

impl LifecycleCause {
    pub const ALL: [Self; 13] = [
        Self::Startup,
        Self::SubscriptionAccepted,
        Self::ResynchronizationStarted,
        Self::SnapshotApplied,
        Self::SourceRecovered,
        Self::SourceStale,
        Self::SequenceGap,
        Self::CapacityRejected,
        Self::RecoverableDisconnect,
        Self::BackoffElapsed,
        Self::SupervisorCommand,
        Self::SchemaViolation,
        Self::Shutdown,
    ];

    pub fn is_compatible(self, from: ConnectorState, to: ConnectorState) -> bool {
        valid_cause(from, to, self)
    }
}

/// One connector lifecycle observation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LifecycleEvent {
    source: SourceId,
    connection_epoch: NonZeroU64,
    sequence: NonZeroU64,
    observed_at: UnixNanos,
    from: ConnectorState,
    to: ConnectorState,
    cause: LifecycleCause,
}

impl LifecycleEvent {
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

    pub const fn from(&self) -> ConnectorState {
        self.from
    }

    pub const fn to(&self) -> ConnectorState {
        self.to
    }

    pub const fn cause(&self) -> LifecycleCause {
        self.cause
    }
}

/// Authoritative, consecutive lifecycle history for one connection epoch.
pub struct LifecycleTracker {
    source: SourceId,
    connection_epoch: NonZeroU64,
    state: ConnectorState,
    next_sequence: NonZeroU64,
}

impl LifecycleTracker {
    pub fn new(source: SourceId, connection_epoch: NonZeroU64) -> Self {
        Self {
            source,
            connection_epoch,
            state: ConnectorState::Stopped,
            next_sequence: NonZeroU64::MIN,
        }
    }

    pub const fn state(&self) -> ConnectorState {
        self.state
    }

    pub fn prepare(
        &self,
        to: ConnectorState,
        cause: LifecycleCause,
        observed_at: UnixNanos,
    ) -> Result<PreparedLifecycleTransition, LifecycleError> {
        self.state.transition_to(to)?;
        if !valid_cause(self.state, to, cause) {
            return Err(LifecycleError::IncompatibleCause);
        }
        let event = LifecycleEvent {
            source: self.source.clone(),
            connection_epoch: self.connection_epoch,
            sequence: self.next_sequence,
            observed_at,
            from: self.state,
            to,
            cause,
        };
        let next_sequence = NonZeroU64::new(
            self.next_sequence
                .get()
                .checked_add(1)
                .ok_or(LifecycleError::SequenceExhausted)?,
        )
        .ok_or(LifecycleError::SequenceExhausted)?;
        Ok(PreparedLifecycleTransition {
            event,
            next_sequence,
        })
    }

    pub fn commit(&mut self, prepared: PreparedLifecycleTransition) -> Result<(), LifecycleError> {
        if prepared.event.source != self.source
            || prepared.event.connection_epoch != self.connection_epoch
            || prepared.event.from != self.state
            || prepared.event.sequence != self.next_sequence
        {
            return Err(LifecycleError::InvalidTransition {
                from: self.state,
                to: prepared.event.to,
            });
        }
        self.state = prepared.event.to;
        self.next_sequence = prepared.next_sequence;
        Ok(())
    }
}

pub struct PreparedLifecycleTransition {
    event: LifecycleEvent,
    next_sequence: NonZeroU64,
}

impl PreparedLifecycleTransition {
    pub const fn event(&self) -> &LifecycleEvent {
        &self.event
    }
}

fn valid_cause(from: ConnectorState, to: ConnectorState, cause: LifecycleCause) -> bool {
    use ConnectorState::{AuthenticatingOrSubscribing as Auth, *};
    match cause {
        LifecycleCause::Startup => from == Stopped && to == Connecting,
        LifecycleCause::SubscriptionAccepted => {
            matches!((from, to), (Connecting, Auth) | (Auth, Synchronizing))
        }
        LifecycleCause::ResynchronizationStarted => from == Recovering && to == Synchronizing,
        LifecycleCause::SnapshotApplied => from == Synchronizing && to == Healthy,
        LifecycleCause::SourceRecovered => from == Degraded && to == Healthy,
        LifecycleCause::SourceStale => to == Degraded,
        LifecycleCause::SequenceGap => to == Recovering,
        LifecycleCause::CapacityRejected => matches!(to, BackingOff | Recovering),
        LifecycleCause::RecoverableDisconnect => to == BackingOff,
        LifecycleCause::BackoffElapsed => from == BackingOff && to == Connecting,
        LifecycleCause::SupervisorCommand => {
            matches!(to, Connecting | Recovering | Quarantined)
        }
        LifecycleCause::SchemaViolation => matches!(to, Degraded | BackingOff | Quarantined),
        LifecycleCause::Shutdown => to == Stopped,
    }
}
