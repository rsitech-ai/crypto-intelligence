//! Pure, externally driven logical time and deterministic timer ordering.

use std::{collections::BTreeMap, num::NonZeroU64};

use domain::UnixNanos;
use thiserror::Error;

const MAX_TIMER_CAPACITY: usize = 4_096;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct TimerId(NonZeroU64);

impl TimerId {
    pub fn new(value: u64) -> Result<Self, ClockError> {
        NonZeroU64::new(value)
            .map(Self)
            .ok_or(ClockError::InvalidTimerId)
    }

    pub const fn value(self) -> u64 {
        self.0.get()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LogicalClock {
    now: UnixNanos,
    capacity: usize,
    timers: BTreeMap<(UnixNanos, TimerId), ()>,
    deadlines: BTreeMap<TimerId, UnixNanos>,
}

impl LogicalClock {
    pub fn try_new(now: UnixNanos, capacity: usize) -> Result<Self, ClockError> {
        if now.value() <= 0 || capacity == 0 || capacity > MAX_TIMER_CAPACITY {
            return Err(ClockError::InvalidClock);
        }
        Ok(Self {
            now,
            capacity,
            timers: BTreeMap::new(),
            deadlines: BTreeMap::new(),
        })
    }

    pub const fn now(&self) -> UnixNanos {
        self.now
    }

    pub fn schedule(&mut self, id: TimerId, deadline: UnixNanos) -> Result<(), ClockError> {
        if deadline < self.now {
            return Err(ClockError::DeadlineBeforeNow);
        }
        if self.deadlines.contains_key(&id) {
            return Err(ClockError::DuplicateTimer);
        }
        if self.deadlines.len() >= self.capacity {
            return Err(ClockError::TimerCapacity);
        }
        self.deadlines.insert(id, deadline);
        self.timers.insert((deadline, id), ());
        Ok(())
    }

    pub fn cancel(&mut self, id: TimerId) -> bool {
        let Some(deadline) = self.deadlines.remove(&id) else {
            return false;
        };
        self.timers.remove(&(deadline, id));
        true
    }

    pub fn advance_to(&mut self, target: UnixNanos) -> Result<Vec<TimerId>, ClockError> {
        if target < self.now {
            return Err(ClockError::Regression {
                current: self.now,
                attempted: target,
            });
        }

        let due_keys: Vec<_> = self
            .timers
            .range(..=(target, TimerId(NonZeroU64::MAX)))
            .map(|(key, ())| *key)
            .collect();
        let mut due = Vec::with_capacity(due_keys.len());
        for (deadline, id) in due_keys {
            self.timers.remove(&(deadline, id));
            self.deadlines.remove(&id);
            due.push(id);
        }
        self.now = target;
        Ok(due)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RecordedTimestamp {
    event_time: UnixNanos,
    receive_time: UnixNanos,
}

impl RecordedTimestamp {
    pub fn try_new(event_time: UnixNanos, receive_time: UnixNanos) -> Result<Self, ClockError> {
        if event_time.value() <= 0 || receive_time.value() <= 0 {
            return Err(ClockError::InvalidRecordedTimestamp);
        }
        Ok(Self {
            event_time,
            receive_time,
        })
    }

    pub const fn event_time(self) -> UnixNanos {
        self.event_time
    }

    pub const fn receive_time(self) -> UnixNanos {
        self.receive_time
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClockBasis {
    EventTime,
    ReceiveTime,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecordedClock {
    logical: LogicalClock,
    basis: ClockBasis,
}

impl RecordedClock {
    pub const fn try_new(logical: LogicalClock, basis: ClockBasis) -> Self {
        Self { logical, basis }
    }

    pub fn schedule(&mut self, id: TimerId, deadline: UnixNanos) -> Result<(), ClockError> {
        self.logical.schedule(id, deadline)
    }

    pub fn cancel(&mut self, id: TimerId) -> bool {
        self.logical.cancel(id)
    }

    pub fn advance(&mut self, timestamp: RecordedTimestamp) -> Result<Vec<TimerId>, ClockError> {
        match self.basis {
            ClockBasis::EventTime => {
                let target = timestamp.event_time();
                if target < self.logical.now() {
                    Ok(Vec::new())
                } else {
                    self.logical.advance_to(target)
                }
            }
            ClockBasis::ReceiveTime => self.logical.advance_to(timestamp.receive_time()),
        }
    }

    pub const fn now(&self) -> UnixNanos {
        self.logical.now()
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum ClockError {
    #[error("timer ID must be nonzero")]
    InvalidTimerId,
    #[error("invalid logical clock configuration")]
    InvalidClock,
    #[error("recorded event and receive times must be positive")]
    InvalidRecordedTimestamp,
    #[error("timer ID is already scheduled")]
    DuplicateTimer,
    #[error("timer capacity reached")]
    TimerCapacity,
    #[error("timer deadline precedes the current logical time")]
    DeadlineBeforeNow,
    #[error("logical time regressed from {current:?} to {attempted:?}")]
    Regression {
        current: UnixNanos,
        attempted: UnixNanos,
    },
}
