//! Pure, externally clocked fast-state cadence.

use std::collections::VecDeque;

use crate::FastStateError;

const INTERNAL_INTERVAL_NS: i64 = 100_000_000;
const PUBLISHED_INTERVAL_NS: i64 = 1_000_000_000;
const MAX_PENDING_TICKS: usize = 65_536;

/// The two fixed Phase 4 fast-state cadence boundaries.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum TickKind {
    Internal100Ms,
    Published1S,
}

/// One immutable scheduled evaluation boundary.
#[derive(Debug, Eq, PartialEq)]
pub struct TickEvent {
    sequence: u64,
    scheduled_at_ns: i64,
    kind: TickKind,
}

impl TickEvent {
    pub const fn sequence(&self) -> u64 {
        self.sequence
    }

    pub const fn scheduled_at_ns(&self) -> i64 {
        self.scheduled_at_ns
    }

    pub const fn kind(&self) -> TickKind {
        self.kind
    }
}

/// Relationship between an accepted event timestamp and current logical time.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EventTimeliness {
    OnTime,
    Late,
}

/// Bounded deterministic scheduler for authoritative compute work.
///
/// The scheduler never reads wall time. Every due tick is enqueued or the
/// advance fails before mutation, so callers must chunk replay advances when
/// the bounded compute backlog cannot accept the entire interval.
#[derive(Debug, Eq, PartialEq)]
pub struct FastStateScheduler {
    now_ns: i64,
    pending_capacity: usize,
    pending: VecDeque<TickEvent>,
    internal_count: u64,
    published_count: u64,
    last_sequence: u64,
    cancelled: bool,
}

impl FastStateScheduler {
    pub fn try_new(start_ns: i64, pending_capacity: usize) -> Result<Self, FastStateError> {
        if start_ns < 0 || pending_capacity == 0 || pending_capacity > MAX_PENDING_TICKS {
            return Err(FastStateError::InvalidScheduler);
        }
        Ok(Self {
            now_ns: start_ns,
            pending_capacity,
            pending: VecDeque::with_capacity(pending_capacity),
            internal_count: 0,
            published_count: 0,
            last_sequence: 0,
            cancelled: false,
        })
    }

    pub const fn now_ns(&self) -> i64 {
        self.now_ns
    }

    pub const fn pending_capacity(&self) -> usize {
        self.pending_capacity
    }

    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    pub const fn count(&self, kind: TickKind) -> u64 {
        match kind {
            TickKind::Internal100Ms => self.internal_count,
            TickKind::Published1S => self.published_count,
        }
    }

    pub const fn is_cancelled(&self) -> bool {
        self.cancelled
    }

    pub fn cancel(&mut self) {
        self.cancelled = true;
    }

    pub fn classify_event_time(
        &self,
        event_time_ns: i64,
    ) -> Result<EventTimeliness, FastStateError> {
        if event_time_ns < 0 {
            return Err(FastStateError::InvalidEventTime);
        }
        if event_time_ns > self.now_ns {
            return Err(FastStateError::EventAheadOfScheduler);
        }
        Ok(if event_time_ns == self.now_ns {
            EventTimeliness::OnTime
        } else {
            EventTimeliness::Late
        })
    }

    pub fn advance_to(&mut self, target_ns: i64) -> Result<(), FastStateError> {
        if self.cancelled {
            return Err(FastStateError::Cancelled);
        }
        if target_ns < self.now_ns {
            return Err(FastStateError::ClockRegression {
                current_ns: self.now_ns,
                attempted_ns: target_ns,
            });
        }

        let due_internal = due_count(self.now_ns, target_ns, INTERNAL_INTERVAL_NS)?;
        let due_published = due_count(self.now_ns, target_ns, PUBLISHED_INTERVAL_NS)?;
        let due_total = due_internal
            .checked_add(due_published)
            .ok_or(FastStateError::CounterOverflow)?;
        let due_total =
            usize::try_from(due_total).map_err(|_| FastStateError::TickBacklogCapacity)?;
        if due_total > self.pending_capacity - self.pending.len() {
            return Err(FastStateError::TickBacklogCapacity);
        }

        let final_internal = self
            .internal_count
            .checked_add(due_internal)
            .ok_or(FastStateError::CounterOverflow)?;
        let final_published = self
            .published_count
            .checked_add(due_published)
            .ok_or(FastStateError::CounterOverflow)?;
        let final_sequence = self
            .last_sequence
            .checked_add(u64::try_from(due_total).map_err(|_| FastStateError::CounterOverflow)?)
            .ok_or(FastStateError::CounterOverflow)?;

        let mut events = Vec::with_capacity(due_total);
        let mut next_internal = 0;
        let mut next_published = 0;
        let mut sequence = self.last_sequence;
        while next_internal < due_internal || next_published < due_published {
            let internal_deadline = if next_internal < due_internal {
                Some(deadline_after(
                    self.now_ns,
                    next_internal,
                    INTERNAL_INTERVAL_NS,
                )?)
            } else {
                None
            };
            let published_deadline = if next_published < due_published {
                Some(deadline_after(
                    self.now_ns,
                    next_published,
                    PUBLISHED_INTERVAL_NS,
                )?)
            } else {
                None
            };
            let (scheduled_at_ns, kind) = match (internal_deadline, published_deadline) {
                (Some(internal), Some(published)) if internal <= published => {
                    next_internal += 1;
                    (internal, TickKind::Internal100Ms)
                }
                (Some(_), Some(published)) => {
                    next_published += 1;
                    (published, TickKind::Published1S)
                }
                (Some(internal), None) => {
                    next_internal += 1;
                    (internal, TickKind::Internal100Ms)
                }
                (None, Some(published)) => {
                    next_published += 1;
                    (published, TickKind::Published1S)
                }
                (None, None) => break,
            };
            sequence = sequence
                .checked_add(1)
                .ok_or(FastStateError::CounterOverflow)?;
            events.push(TickEvent {
                sequence,
                scheduled_at_ns,
                kind,
            });
        }

        self.pending.extend(events);
        self.internal_count = final_internal;
        self.published_count = final_published;
        self.last_sequence = final_sequence;
        self.now_ns = target_ns;
        Ok(())
    }

    pub fn drain_ticks(&mut self, maximum: usize) -> Vec<TickEvent> {
        let count = maximum.min(self.pending.len());
        self.pending.drain(..count).collect()
    }
}

fn due_count(current_ns: i64, target_ns: i64, interval_ns: i64) -> Result<u64, FastStateError> {
    let current_ticks = current_ns / interval_ns;
    let target_ticks = target_ns / interval_ns;
    u64::try_from(target_ticks - current_ticks).map_err(|_| FastStateError::CounterOverflow)
}

fn deadline_after(
    current_ns: i64,
    completed_count: u64,
    interval_ns: i64,
) -> Result<i64, FastStateError> {
    let current_ordinal = current_ns / interval_ns;
    let ordinal = i128::from(current_ordinal)
        .checked_add(i128::from(completed_count))
        .and_then(|value| value.checked_add(1))
        .ok_or(FastStateError::CounterOverflow)?;
    let value = ordinal
        .checked_mul(i128::from(interval_ns))
        .ok_or(FastStateError::CounterOverflow)?;
    i64::try_from(value).map_err(|_| FastStateError::CounterOverflow)
}
