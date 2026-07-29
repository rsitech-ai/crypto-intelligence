//! Pure supervisor core; connector tasks retain transport and channel ownership.

use std::{
    collections::{BTreeSet, HashMap, VecDeque},
    future::Future,
    num::{NonZeroU32, NonZeroU64, NonZeroUsize},
};

use connector_core::{ConnectorCommand, ResynchronizationReason};
use domain::{SourceId, SourceKind, UnixNanos};
use quality::{QualityCause, QualityError, SourceHealthState, SourceHealthTracker};
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const MAX_SUPERVISED_SOURCES: usize = 4_096;
pub const MAX_QUEUE_CAPACITY: usize = 1_048_576;
pub const MAX_INSTRUMENTS_PER_TIER: u32 = 10_000;
pub const MAX_RETRY_ATTEMPTS: u32 = 64;
pub const MAX_RETRY_DELAY_MS: u64 = 86_400_000;
pub const MAX_PENDING_QUALITY_EVENTS: usize = 65_536;
const MAX_JITTER_BASIS_POINTS: u16 = 10_000;
const QUALITY_EVENTS_PER_SOURCE: usize = 16;

/// Priority/persistence class for a bounded handoff.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueueClass {
    InstrumentDefinition,
    BookSnapshot,
    BookDelta,
    Trade,
    MarkIndex,
    DerivativesState,
    OptionalTicker,
    UiTicker,
    RawWal,
    HistoricalCompaction,
}

impl QueueClass {
    pub const ALL: [Self; 10] = [
        Self::InstrumentDefinition,
        Self::BookSnapshot,
        Self::BookDelta,
        Self::Trade,
        Self::MarkIndex,
        Self::DerivativesState,
        Self::OptionalTicker,
        Self::UiTicker,
        Self::RawWal,
        Self::HistoricalCompaction,
    ];

    pub const fn coalescing_allowed(self) -> bool {
        matches!(self, Self::OptionalTicker | Self::UiTicker)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OverflowAction {
    Backpressure,
    CoalesceByKey,
    DegradeAndResnapshot,
    RejectAdmission,
}

pub const fn overflow_action(class: QueueClass) -> OverflowAction {
    match class {
        QueueClass::BookDelta => OverflowAction::DegradeAndResnapshot,
        QueueClass::OptionalTicker | QueueClass::UiTicker => OverflowAction::CoalesceByKey,
        QueueClass::HistoricalCompaction => OverflowAction::RejectAdmission,
        QueueClass::InstrumentDefinition
        | QueueClass::BookSnapshot
        | QueueClass::Trade
        | QueueClass::MarkIndex
        | QueueClass::DerivativesState
        | QueueClass::RawWal => OverflowAction::Backpressure,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OfferOutcome {
    Queued,
    Coalesced,
    Backpressured,
    Resynchronize,
}

/// Bounded counters exported for every queue instance.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct QueueMetrics {
    capacity: usize,
    occupancy: usize,
    accepted: u64,
    completed: u64,
    overflows: u64,
    coalesced: u64,
    backpressured: u64,
    resynchronizations: u64,
    rejected: u64,
    silent_drops: u64,
}

impl QueueMetrics {
    pub const fn capacity(self) -> usize {
        self.capacity
    }

    pub const fn occupancy(self) -> usize {
        self.occupancy
    }

    pub const fn accepted(self) -> u64 {
        self.accepted
    }

    pub const fn completed(self) -> u64 {
        self.completed
    }

    pub const fn overflows(self) -> u64 {
        self.overflows
    }

    pub const fn coalesced(self) -> u64 {
        self.coalesced
    }

    pub const fn backpressured(self) -> u64 {
        self.backpressured
    }

    pub const fn resynchronizations(self) -> u64 {
        self.resynchronizations
    }

    pub const fn rejected(self) -> u64 {
        self.rejected
    }

    pub const fn silent_drops(self) -> u64 {
        self.silent_drops
    }
}

/// Occupancy and keyed-coalescing state for one bounded queue.
pub struct QueueTracker {
    class: QueueClass,
    keys: BTreeSet<String>,
    metrics: QueueMetrics,
}

impl QueueTracker {
    pub fn try_new(class: QueueClass, capacity: NonZeroUsize) -> Result<Self, AdmissionError> {
        if capacity.get() > MAX_QUEUE_CAPACITY {
            return Err(AdmissionError::InvalidQueueCapacity);
        }
        Ok(Self {
            class,
            keys: BTreeSet::new(),
            metrics: QueueMetrics {
                capacity: capacity.get(),
                occupancy: 0,
                accepted: 0,
                completed: 0,
                overflows: 0,
                coalesced: 0,
                backpressured: 0,
                resynchronizations: 0,
                rejected: 0,
                silent_drops: 0,
            },
        })
    }

    pub const fn class(&self) -> QueueClass {
        self.class
    }

    pub const fn metrics(&self) -> QueueMetrics {
        self.metrics
    }

    pub const fn overflow_action(&self) -> OverflowAction {
        overflow_action(self.class)
    }

    pub fn offer(&mut self, key: Option<&str>) -> Result<OfferOutcome, AdmissionError> {
        let key = if self.class.coalescing_allowed() {
            Some(validate_queue_key(key)?)
        } else {
            None
        };
        if key.is_some_and(|value| self.keys.contains(value)) {
            let overflows = checked_increment(self.metrics.overflows)?;
            let coalesced = checked_increment(self.metrics.coalesced)?;
            self.metrics.overflows = overflows;
            self.metrics.coalesced = coalesced;
            return Ok(OfferOutcome::Coalesced);
        }
        if self.metrics.occupancy < self.metrics.capacity {
            let occupancy = self
                .metrics
                .occupancy
                .checked_add(1)
                .ok_or(AdmissionError::CounterExhausted)?;
            let accepted = checked_increment(self.metrics.accepted)?;
            if let Some(key) = key {
                self.keys.insert(key.to_owned());
            }
            self.metrics.occupancy = occupancy;
            self.metrics.accepted = accepted;
            return Ok(OfferOutcome::Queued);
        }

        let overflows = checked_increment(self.metrics.overflows)?;
        match overflow_action(self.class) {
            OverflowAction::Backpressure => {
                let backpressured = checked_increment(self.metrics.backpressured)?;
                self.metrics.overflows = overflows;
                self.metrics.backpressured = backpressured;
                Ok(OfferOutcome::Backpressured)
            }
            OverflowAction::DegradeAndResnapshot => {
                let resynchronizations = checked_increment(self.metrics.resynchronizations)?;
                self.metrics.overflows = overflows;
                self.metrics.resynchronizations = resynchronizations;
                Ok(OfferOutcome::Resynchronize)
            }
            OverflowAction::CoalesceByKey | OverflowAction::RejectAdmission => {
                let rejected = checked_increment(self.metrics.rejected)?;
                self.metrics.overflows = overflows;
                self.metrics.rejected = rejected;
                Err(AdmissionError::QueueCapacity)
            }
        }
    }

    pub fn complete(&mut self, key: Option<&str>) -> Result<(), AdmissionError> {
        if self.metrics.occupancy == 0 {
            return Err(AdmissionError::QueueEmpty);
        }
        let completed = checked_increment(self.metrics.completed)?;
        let occupancy = self
            .metrics
            .occupancy
            .checked_sub(1)
            .ok_or(AdmissionError::CounterExhausted)?;
        if self.class.coalescing_allowed() {
            let key = validate_queue_key(key)?;
            if !self.keys.contains(key) {
                return Err(AdmissionError::UnknownQueueKey);
            }
            self.keys.remove(key);
        }
        self.metrics.occupancy = occupancy;
        self.metrics.completed = completed;
        Ok(())
    }
}

fn validate_queue_key(key: Option<&str>) -> Result<&str, AdmissionError> {
    let key = key.ok_or(AdmissionError::MissingCoalescingKey)?;
    if key.is_empty() || key.len() > 256 || key.chars().any(char::is_control) {
        Err(AdmissionError::InvalidCoalescingKey)
    } else {
        Ok(key)
    }
}

fn checked_increment(counter: u64) -> Result<u64, AdmissionError> {
    counter
        .checked_add(1)
        .ok_or(AdmissionError::CounterExhausted)
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CoverageTier {
    A,
    B,
    C,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SourcePolicy {
    tier: CoverageTier,
    instruments: NonZeroU32,
}

impl SourcePolicy {
    pub const fn new(tier: CoverageTier, instruments: NonZeroU32) -> Self {
        Self { tier, instruments }
    }

    pub const fn tier(self) -> CoverageTier {
        self.tier
    }

    pub const fn instruments(self) -> NonZeroU32 {
        self.instruments
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AdmissionLimits {
    tier_a_instruments: u32,
    tier_b_instruments: u32,
    tier_c_instruments: u32,
    source_count: usize,
}

impl AdmissionLimits {
    pub fn try_new(
        tier_a_instruments: u32,
        tier_b_instruments: u32,
        tier_c_instruments: u32,
        source_count: usize,
    ) -> Result<Self, AdmissionError> {
        if source_count == 0
            || source_count > MAX_SUPERVISED_SOURCES
            || [tier_a_instruments, tier_b_instruments, tier_c_instruments]
                .into_iter()
                .any(|limit| limit > MAX_INSTRUMENTS_PER_TIER)
        {
            return Err(AdmissionError::InvalidLimits);
        }
        Ok(Self {
            tier_a_instruments,
            tier_b_instruments,
            tier_c_instruments,
            source_count,
        })
    }

    const fn for_tier(self, tier: CoverageTier) -> u32 {
        match tier {
            CoverageTier::A => self.tier_a_instruments,
            CoverageTier::B => self.tier_b_instruments,
            CoverageTier::C => self.tier_c_instruments,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RetryPolicy {
    maximum_attempts: NonZeroU32,
    base_delay_ms: NonZeroU64,
    maximum_delay_ms: NonZeroU64,
    jitter_basis_points: u16,
    seed: u64,
}

impl RetryPolicy {
    pub fn try_new(
        maximum_attempts: u32,
        base_delay_ms: u64,
        maximum_delay_ms: u64,
        jitter_basis_points: u16,
        seed: u64,
    ) -> Result<Self, AdmissionError> {
        let maximum_attempts =
            NonZeroU32::new(maximum_attempts).ok_or(AdmissionError::InvalidRetryPolicy)?;
        let base_delay_ms =
            NonZeroU64::new(base_delay_ms).ok_or(AdmissionError::InvalidRetryPolicy)?;
        let maximum_delay_ms =
            NonZeroU64::new(maximum_delay_ms).ok_or(AdmissionError::InvalidRetryPolicy)?;
        if maximum_attempts.get() > MAX_RETRY_ATTEMPTS
            || maximum_delay_ms < base_delay_ms
            || maximum_delay_ms.get() > MAX_RETRY_DELAY_MS
            || jitter_basis_points > MAX_JITTER_BASIS_POINTS
        {
            return Err(AdmissionError::InvalidRetryPolicy);
        }
        Ok(Self {
            maximum_attempts,
            base_delay_ms,
            maximum_delay_ms,
            jitter_basis_points,
            seed,
        })
    }

    fn delay_ms(self, source: &SourceId, epoch: NonZeroU64, attempt: NonZeroU32) -> u64 {
        let exponent = attempt.get().saturating_sub(1).min(63);
        let unjittered = self
            .base_delay_ms
            .get()
            .saturating_mul(1_u64 << exponent)
            .min(self.maximum_delay_ms.get());
        if self.jitter_basis_points == 0 {
            return unjittered;
        }
        let span = unjittered.saturating_mul(u64::from(self.jitter_basis_points))
            / u64::from(MAX_JITTER_BASIS_POINTS);
        let lower = unjittered.saturating_sub(span);
        let upper = unjittered
            .saturating_add(span)
            .min(self.maximum_delay_ms.get());
        let bucket_count = upper - lower + 1;
        lower + stable_retry_hash(self.seed, source, epoch, attempt) % bucket_count
    }
}

fn stable_retry_hash(seed: u64, source: &SourceId, epoch: NonZeroU64, attempt: NonZeroU32) -> u64 {
    let mut value = seed ^ epoch.get().rotate_left(17) ^ u64::from(attempt.get()).rotate_left(41);
    value ^= u64::from(source.kind() as u8);
    value ^= u64::from(source.generation()).rotate_left(29);
    for byte in source.name().bytes() {
        value ^= u64::from(byte);
        value = value.wrapping_mul(0x100_0000_01b3);
    }
    mix64(value)
}

const fn mix64(mut value: u64) -> u64 {
    value ^= value >> 30;
    value = value.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value ^= value >> 27;
    value = value.wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RetryDecision {
    Backoff { attempt: NonZeroU32, delay_ms: u64 },
    Quarantined,
}

struct SourceEntry {
    policy: SourcePolicy,
    retry: RetryPolicy,
    failed_attempts: u32,
    last_failure_at: Option<UnixNanos>,
    epoch: NonZeroU64,
    next_command_id: NonZeroU64,
    quality: SourceHealthTracker,
}

pub struct CollectorSupervisor {
    limits: AdmissionLimits,
    admitted_by_tier: [u32; 3],
    sources: HashMap<SourceId, SourceEntry>,
    quality_events: VecDeque<quality::QualityEvent>,
    quality_event_capacity: usize,
}

impl CollectorSupervisor {
    pub fn try_new(limits: AdmissionLimits) -> Result<Self, AdmissionError> {
        let quality_event_capacity = limits
            .source_count
            .checked_mul(QUALITY_EVENTS_PER_SOURCE)
            .filter(|capacity| *capacity <= MAX_PENDING_QUALITY_EVENTS)
            .ok_or(AdmissionError::InvalidLimits)?;
        Ok(Self {
            limits,
            admitted_by_tier: [0; 3],
            sources: HashMap::new(),
            quality_events: VecDeque::with_capacity(quality_event_capacity),
            quality_event_capacity,
        })
    }

    pub fn admit(
        &mut self,
        source: SourceId,
        policy: SourcePolicy,
        retry: RetryPolicy,
    ) -> Result<(), AdmissionError> {
        self.admit_with_epoch(source, policy, retry, NonZeroU64::MIN)
    }

    pub fn admit_with_epoch(
        &mut self,
        source: SourceId,
        policy: SourcePolicy,
        retry: RetryPolicy,
        epoch: NonZeroU64,
    ) -> Result<(), AdmissionError> {
        if self.sources.contains_key(&source) {
            return Err(AdmissionError::DuplicateSource);
        }
        if self.sources.len() >= self.limits.source_count {
            return Err(AdmissionError::SourceCapacity);
        }
        let index = tier_index(policy.tier);
        let admitted = self.admitted_by_tier[index]
            .checked_add(policy.instruments.get())
            .ok_or(AdmissionError::CounterExhausted)?;
        if admitted > self.limits.for_tier(policy.tier) {
            return Err(AdmissionError::TierCapacity { tier: policy.tier });
        }
        self.sources.insert(
            source.clone(),
            SourceEntry {
                policy,
                retry,
                failed_attempts: 0,
                last_failure_at: None,
                epoch,
                next_command_id: NonZeroU64::MIN,
                quality: SourceHealthTracker::new(source, epoch),
            },
        );
        self.admitted_by_tier[index] = admitted;
        Ok(())
    }

    pub fn source_count(&self) -> usize {
        self.sources.len()
    }

    pub fn pending_quality_events(&self) -> usize {
        self.quality_events.len()
    }

    pub fn pop_quality_event(&mut self) -> Option<quality::QualityEvent> {
        self.quality_events.pop_front()
    }

    pub fn source_state(&self, source: &SourceId) -> Result<SourceHealthState, AdmissionError> {
        self.sources
            .get(source)
            .map(|entry| entry.quality.state())
            .ok_or(AdmissionError::UnknownSource)
    }

    pub fn mark_healthy(
        &mut self,
        source: &SourceId,
        observed_at: UnixNanos,
    ) -> Result<(), AdmissionError> {
        ensure_quality_capacity(&self.quality_events, self.quality_event_capacity, 1)?;
        let entry = self
            .sources
            .get_mut(source)
            .ok_or(AdmissionError::UnknownSource)?;
        publish_quality_transition(
            &mut self.quality_events,
            &mut entry.quality,
            SourceHealthState::Healthy,
            QualityCause::RecoveryVerified,
            observed_at,
        )?;
        entry.failed_attempts = 0;
        entry.last_failure_at = None;
        Ok(())
    }

    pub fn record_failure(
        &mut self,
        source: &SourceId,
        observed_at: UnixNanos,
    ) -> Result<RetryDecision, AdmissionError> {
        let entry = self
            .sources
            .get(source)
            .ok_or(AdmissionError::UnknownSource)?;
        if observed_at.value() <= 0
            || entry
                .last_failure_at
                .is_some_and(|previous| observed_at <= previous)
        {
            return Err(AdmissionError::NonMonotonicFailureTime);
        }
        let next_attempt = entry
            .failed_attempts
            .checked_add(1)
            .ok_or(AdmissionError::CounterExhausted)?;
        let quality_event_count =
            usize::from(entry.quality.state() != SourceHealthState::Unhealthy)
                + usize::from(next_attempt >= entry.retry.maximum_attempts.get());
        let quarantine_at = if next_attempt >= entry.retry.maximum_attempts.get() {
            Some(UnixNanos::new(
                observed_at
                    .value()
                    .checked_add(1)
                    .ok_or(AdmissionError::ObservationTimeExhausted)?,
            ))
        } else {
            None
        };
        ensure_quality_capacity(
            &self.quality_events,
            self.quality_event_capacity,
            quality_event_count,
        )?;
        let entry = self
            .sources
            .get_mut(source)
            .ok_or(AdmissionError::UnknownSource)?;
        entry
            .quality
            .ensure_sequence_capacity(quality_event_count)?;
        if entry.quality.state() != SourceHealthState::Unhealthy {
            publish_quality_transition(
                &mut self.quality_events,
                &mut entry.quality,
                SourceHealthState::Unhealthy,
                QualityCause::SequenceIntegrityFailed,
                observed_at,
            )?;
        }
        entry.failed_attempts = entry
            .failed_attempts
            .checked_add(1)
            .ok_or(AdmissionError::CounterExhausted)?;
        entry.last_failure_at = Some(observed_at);
        if entry.failed_attempts >= entry.retry.maximum_attempts.get() {
            publish_quality_transition(
                &mut self.quality_events,
                &mut entry.quality,
                SourceHealthState::Quarantined,
                QualityCause::RetryBudgetExhausted,
                quarantine_at.ok_or(AdmissionError::ObservationTimeExhausted)?,
            )?;
            return Ok(RetryDecision::Quarantined);
        }
        let attempt =
            NonZeroU32::new(entry.failed_attempts).ok_or(AdmissionError::CounterExhausted)?;
        Ok(RetryDecision::Backoff {
            attempt,
            delay_ms: entry.retry.delay_ms(source, entry.epoch, attempt),
        })
    }

    pub fn renew_connection(&mut self, source: &SourceId) -> Result<NonZeroU64, AdmissionError> {
        let entry = self
            .sources
            .get_mut(source)
            .ok_or(AdmissionError::UnknownSource)?;
        let epoch = entry
            .epoch
            .get()
            .checked_add(1)
            .and_then(NonZeroU64::new)
            .ok_or(AdmissionError::ConnectionEpochExhausted)?;
        entry.epoch = epoch;
        entry.failed_attempts = 0;
        entry.last_failure_at = None;
        entry.quality = SourceHealthTracker::new(source.clone(), epoch);
        Ok(epoch)
    }

    pub fn remove(&mut self, source: &SourceId) -> Result<SourcePolicy, AdmissionError> {
        let policy = self
            .sources
            .get(source)
            .map(|entry| entry.policy)
            .ok_or(AdmissionError::UnknownSource)?;
        let index = tier_index(policy.tier);
        let remaining = self.admitted_by_tier[index]
            .checked_sub(policy.instruments.get())
            .ok_or(AdmissionError::CounterExhausted)?;
        self.sources.remove(source);
        self.admitted_by_tier[index] = remaining;
        Ok(policy)
    }

    pub fn handle_book_overflow(
        &mut self,
        source: &SourceId,
        degraded_at: UnixNanos,
        recovering_at: UnixNanos,
    ) -> Result<OverflowReport, AdmissionError> {
        if degraded_at.value() <= 0 || recovering_at <= degraded_at {
            return Err(AdmissionError::NonMonotonicFailureTime);
        }
        ensure_quality_capacity(&self.quality_events, self.quality_event_capacity, 2)?;
        let entry = self
            .sources
            .get_mut(source)
            .ok_or(AdmissionError::UnknownSource)?;
        entry.quality.ensure_sequence_capacity(2)?;
        let next_command_id = entry
            .next_command_id
            .get()
            .checked_add(1)
            .and_then(NonZeroU64::new)
            .ok_or(AdmissionError::CommandSequenceExhausted)?;
        publish_quality_transition(
            &mut self.quality_events,
            &mut entry.quality,
            SourceHealthState::Degraded,
            QualityCause::CapacityPressure,
            degraded_at,
        )?;
        publish_quality_transition(
            &mut self.quality_events,
            &mut entry.quality,
            SourceHealthState::Recovering,
            QualityCause::RecoveryStarted,
            recovering_at,
        )?;
        let command_id = entry.next_command_id;
        entry.next_command_id = next_command_id;
        Ok(OverflowReport {
            action: OverflowAction::DegradeAndResnapshot,
            silent_drops: 0,
            published_quality_events: 2,
            quality_state: entry.quality.state(),
            connector_command: Some(ConnectorCommand::Resynchronize {
                id: command_id,
                reason: ResynchronizationReason::RequiredDeltaOverflow,
            }),
        })
    }
}

fn ensure_quality_capacity(
    events: &VecDeque<quality::QualityEvent>,
    capacity: usize,
    additional: usize,
) -> Result<(), AdmissionError> {
    if events
        .len()
        .checked_add(additional)
        .is_some_and(|required| required <= capacity)
    {
        Ok(())
    } else {
        Err(AdmissionError::QualityPublicationBackpressure)
    }
}

fn publish_quality_transition(
    events: &mut VecDeque<quality::QualityEvent>,
    tracker: &mut SourceHealthTracker,
    to: SourceHealthState,
    cause: QualityCause,
    observed_at: UnixNanos,
) -> Result<(), AdmissionError> {
    let prepared = tracker.prepare(to, cause, observed_at)?;
    let event = prepared.event().clone();
    tracker.commit(prepared)?;
    events.push_back(event);
    Ok(())
}

const fn tier_index(tier: CoverageTier) -> usize {
    match tier {
        CoverageTier::A => 0,
        CoverageTier::B => 1,
        CoverageTier::C => 2,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OverflowReport {
    action: OverflowAction,
    silent_drops: u64,
    published_quality_events: usize,
    quality_state: SourceHealthState,
    connector_command: Option<ConnectorCommand>,
}

impl OverflowReport {
    pub const fn action(self) -> OverflowAction {
        self.action
    }

    pub const fn silent_drops(self) -> u64 {
        self.silent_drops
    }

    pub const fn published_quality_events(self) -> usize {
        self.published_quality_events
    }

    pub const fn quality_state(self) -> SourceHealthState {
        self.quality_state
    }

    pub const fn connector_command(self) -> Option<ConnectorCommand> {
        self.connector_command
    }
}

/// Deterministic harness retained only to exercise the public supervisor path.
pub struct SupervisorHarness {
    supervisor: CollectorSupervisor,
    source: SourceId,
    book_deltas: QueueTracker,
}

impl SupervisorHarness {
    pub fn with_capacity(capacity: usize) -> Result<Self, AdmissionError> {
        let capacity = NonZeroUsize::new(capacity).ok_or(AdmissionError::InvalidQueueCapacity)?;
        let source = SourceId::new(SourceKind::Exchange, "supervisor-harness", 1)
            .map_err(|_| AdmissionError::InvalidSource)?;
        let mut supervisor = CollectorSupervisor::try_new(AdmissionLimits::try_new(1, 0, 0, 1)?)?;
        supervisor.admit(
            source.clone(),
            SourcePolicy::new(CoverageTier::A, NonZeroU32::MIN),
            RetryPolicy::try_new(3, 100, 1_000, 0, 1)?,
        )?;
        supervisor.mark_healthy(&source, UnixNanos::new(1))?;
        let _ = supervisor.pop_quality_event();
        Ok(Self {
            supervisor,
            source,
            book_deltas: QueueTracker::try_new(QueueClass::BookDelta, capacity)?,
        })
    }

    pub async fn saturate_book_deltas(&mut self) -> Result<OverflowReport, AdmissionError> {
        if self.book_deltas.metrics().occupancy == 0 {
            let outcome = self.book_deltas.offer(None)?;
            if outcome != OfferOutcome::Queued {
                return Err(AdmissionError::UnexpectedQueueOutcome);
            }
        }
        let outcome = self.book_deltas.offer(None)?;
        if outcome != OfferOutcome::Resynchronize {
            return Err(AdmissionError::UnexpectedQueueOutcome);
        }
        self.supervisor
            .handle_book_overflow(&self.source, UnixNanos::new(2), UnixNanos::new(3))
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShutdownPhase {
    StopSourceReads,
    DrainWal,
    CheckpointBooks,
    SealPartitions,
    CloseMetadata,
}

pub trait ShutdownActions: Send {
    fn stop_source_reads(&mut self) -> impl Future<Output = Result<(), ShutdownError>> + Send;
    fn drain_wal(&mut self) -> impl Future<Output = Result<(), ShutdownError>> + Send;
    fn checkpoint_books(&mut self) -> impl Future<Output = Result<(), ShutdownError>> + Send;
    fn seal_partitions(&mut self) -> impl Future<Output = Result<(), ShutdownError>> + Send;
    fn close_metadata(&mut self) -> impl Future<Output = Result<(), ShutdownError>> + Send;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ShutdownReport {
    completed: Vec<ShutdownPhase>,
}

impl ShutdownReport {
    pub fn completed(&self) -> &[ShutdownPhase] {
        &self.completed
    }
}

pub async fn execute_shutdown(
    actions: &mut impl ShutdownActions,
) -> Result<ShutdownReport, ShutdownError> {
    let mut completed = Vec::with_capacity(5);
    actions.stop_source_reads().await?;
    completed.push(ShutdownPhase::StopSourceReads);
    actions.drain_wal().await?;
    completed.push(ShutdownPhase::DrainWal);
    actions.checkpoint_books().await?;
    completed.push(ShutdownPhase::CheckpointBooks);
    actions.seal_partitions().await?;
    completed.push(ShutdownPhase::SealPartitions);
    actions.close_metadata().await?;
    completed.push(ShutdownPhase::CloseMetadata);
    Ok(ShutdownReport { completed })
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum ShutdownError {
    #[error("shutdown step failed at {0:?}")]
    StepFailed(ShutdownPhase),
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum AdmissionError {
    #[error("queue capacity is invalid")]
    InvalidQueueCapacity,
    #[error("queue capacity is exhausted")]
    QueueCapacity,
    #[error("queue is empty")]
    QueueEmpty,
    #[error("coalescing queue requires a bounded key")]
    MissingCoalescingKey,
    #[error("coalescing queue key is invalid")]
    InvalidCoalescingKey,
    #[error("coalescing queue key is not currently queued")]
    UnknownQueueKey,
    #[error("supervisor limits are invalid")]
    InvalidLimits,
    #[error("retry policy is invalid")]
    InvalidRetryPolicy,
    #[error("source identity is invalid")]
    InvalidSource,
    #[error("source is already supervised")]
    DuplicateSource,
    #[error("source is not supervised")]
    UnknownSource,
    #[error("supervisor source registry is full")]
    SourceCapacity,
    #[error("coverage tier {tier:?} capacity is exhausted")]
    TierCapacity { tier: CoverageTier },
    #[error("bounded counter is exhausted")]
    CounterExhausted,
    #[error("connection epoch is exhausted")]
    ConnectionEpochExhausted,
    #[error("supervisor command sequence is exhausted")]
    CommandSequenceExhausted,
    #[error("quality observation time is exhausted")]
    ObservationTimeExhausted,
    #[error("source failure observation time did not advance")]
    NonMonotonicFailureTime,
    #[error("quality publication queue is full")]
    QualityPublicationBackpressure,
    #[error("queue produced an outcome incompatible with its class")]
    UnexpectedQueueOutcome,
    #[error("source-quality transition failed: {0}")]
    Quality(#[from] QualityError),
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;

    use super::{AdmissionError, QueueClass, QueueTracker};

    #[test]
    fn exhausted_metrics_do_not_partially_mutate_queue_state() {
        let mut queue =
            QueueTracker::try_new(QueueClass::RawWal, NonZeroUsize::MIN).expect("queue");
        queue.metrics.accepted = u64::MAX;
        let before = queue.metrics();

        assert_eq!(queue.offer(None), Err(AdmissionError::CounterExhausted));
        assert_eq!(queue.metrics(), before);
    }
}
