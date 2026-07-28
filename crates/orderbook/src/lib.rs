//! Integrity-first, single-writer order-book reconstruction.

mod book;
mod checksum;
mod state;

pub use book::{BookSnapshotView, L3Event, L3Order, L3OrderId, L3Side};
pub use checksum::{
    ExactChecksumLevel, ExactL3ChecksumOrder, KrakenV2Checksum, KrakenV2L3Checksum,
};
pub use state::{
    ApplyResult, BookClassification, BookConfig, BookQuality, BookSession, BookState,
    ChecksumPolicy, ChecksumStatus, SequencePolicy, SnapshotStrategy, SourceLatencyPercentiles,
};

use book::{L2Book, L3Book};
use event_envelope::{BookDelta, BookSnapshot, EventEnvelope, UncheckedEventPayload};
use state::{HourlyCounter, LatencyWindow};
use std::collections::VecDeque;
use thiserror::Error;

#[derive(Clone, Debug)]
struct BufferedDelta {
    delta: BookDelta,
    session: BookSession,
    now_monotonic_ns: u64,
    checksum: Option<KrakenV2Checksum>,
    previous_final_sequence: Option<u64>,
}

/// Mutable state owned by exactly one instrument shard writer.
pub struct OrderBookEngine {
    config: BookConfig,
    state: BookState,
    session: Option<BookSession>,
    strategy: Option<SnapshotStrategy>,
    book: Option<L2Book>,
    l3: Option<L3Book>,
    buffered: VecDeque<BufferedDelta>,
    buffered_level_updates: usize,
    updated_monotonic_ns: u64,
    last_observed_monotonic_ns: u64,
    checksum_status: ChecksumStatus,
    l3_checksum_status: ChecksumStatus,
    resync_count: HourlyCounter,
    missing_sequence_count: HourlyCounter,
    checksum_failure_count: HourlyCounter,
    crossed_state_count: HourlyCounter,
    source_latency: LatencyWindow,
    resync_in_progress: bool,
}

impl OrderBookEngine {
    pub fn new(config: BookConfig) -> Result<Self, BookError> {
        if !config.validate() {
            return Err(BookError::InvalidConfig);
        }
        let checksum_status = match config.checksum_policy {
            ChecksumPolicy::Disabled => ChecksumStatus::NotSupported,
            ChecksumPolicy::Required => ChecksumStatus::Pending,
        };
        let l3_checksum_status = if config.max_l3_orders.is_some() {
            ChecksumStatus::Pending
        } else {
            ChecksumStatus::NotSupported
        };
        Ok(Self {
            config,
            state: BookState::Disconnected,
            session: None,
            strategy: None,
            book: None,
            l3: None,
            buffered: VecDeque::new(),
            buffered_level_updates: 0,
            updated_monotonic_ns: 0,
            last_observed_monotonic_ns: 0,
            checksum_status,
            l3_checksum_status,
            resync_count: HourlyCounter::default(),
            missing_sequence_count: HourlyCounter::default(),
            checksum_failure_count: HourlyCounter::default(),
            crossed_state_count: HourlyCounter::default(),
            source_latency: LatencyWindow::default(),
            resync_in_progress: false,
        })
    }

    pub const fn state(&self) -> BookState {
        self.state
    }

    pub const fn current_session(&self) -> Option<BookSession> {
        self.session
    }

    pub fn start_session(
        &mut self,
        session: BookSession,
        strategy: SnapshotStrategy,
    ) -> Result<(), BookError> {
        self.validate_new_session(session)?;
        let replacing_session = self.session.is_some();
        self.session = Some(session);
        self.strategy = Some(strategy);
        self.book = None;
        self.l3 = None;
        self.buffered.clear();
        self.buffered_level_updates = 0;
        self.updated_monotonic_ns = 0;
        self.resync_in_progress = replacing_session;
        self.checksum_status = match self.config.checksum_policy {
            ChecksumPolicy::Disabled => ChecksumStatus::NotSupported,
            ChecksumPolicy::Required => ChecksumStatus::Pending,
        };
        self.l3_checksum_status = if self.config.max_l3_orders.is_some() {
            ChecksumStatus::Pending
        } else {
            ChecksumStatus::NotSupported
        };
        self.state = match strategy {
            SnapshotStrategy::ExternalBuffered => BookState::Buffering,
            SnapshotStrategy::StreamSnapshot => BookState::AwaitingSnapshot,
        };
        Ok(())
    }

    pub fn disconnect(&mut self) {
        self.state = BookState::Disconnected;
        self.strategy = None;
        self.book = None;
        self.l3 = None;
        self.buffered.clear();
        self.buffered_level_updates = 0;
        self.updated_monotonic_ns = 0;
        self.resync_in_progress = false;
        self.checksum_status = match self.config.checksum_policy {
            ChecksumPolicy::Disabled => ChecksumStatus::NotSupported,
            ChecksumPolicy::Required => ChecksumStatus::Pending,
        };
        self.l3_checksum_status = if self.config.max_l3_orders.is_some() {
            ChecksumStatus::Pending
        } else {
            ChecksumStatus::NotSupported
        };
    }

    pub fn apply_snapshot(
        &mut self,
        snapshot: BookSnapshot,
        session: BookSession,
        now_monotonic_ns: u64,
    ) -> Result<ApplyResult, BookError> {
        self.apply_snapshot_candidate(snapshot, session, now_monotonic_ns, None, None)
    }

    pub fn apply_snapshot_checked(
        &mut self,
        snapshot: BookSnapshot,
        session: BookSession,
        now_monotonic_ns: u64,
        source_latency_ms: u64,
        checksum: KrakenV2Checksum,
    ) -> Result<ApplyResult, BookError> {
        self.apply_snapshot_candidate(
            snapshot,
            session,
            now_monotonic_ns,
            Some(checksum),
            Some(source_latency_ms),
        )
    }

    pub fn apply_delta(
        &mut self,
        delta: BookDelta,
        session: BookSession,
        now_monotonic_ns: u64,
    ) -> Result<ApplyResult, BookError> {
        self.apply_delta_candidate(delta, session, now_monotonic_ns, None, None, None)
    }

    pub fn apply_delta_with_previous(
        &mut self,
        delta: BookDelta,
        session: BookSession,
        now_monotonic_ns: u64,
        previous_final_sequence: u64,
    ) -> Result<ApplyResult, BookError> {
        self.apply_delta_candidate(
            delta,
            session,
            now_monotonic_ns,
            None,
            Some(previous_final_sequence),
            None,
        )
    }

    pub fn apply_delta_checked(
        &mut self,
        delta: BookDelta,
        session: BookSession,
        now_monotonic_ns: u64,
        source_latency_ms: u64,
        checksum: KrakenV2Checksum,
    ) -> Result<ApplyResult, BookError> {
        self.apply_delta_candidate(
            delta,
            session,
            now_monotonic_ns,
            Some(checksum),
            None,
            Some(source_latency_ms),
        )
    }

    pub fn apply_delta_checked_with_previous(
        &mut self,
        delta: BookDelta,
        session: BookSession,
        now_monotonic_ns: u64,
        source_latency_ms: u64,
        checksum: KrakenV2Checksum,
        previous_final_sequence: u64,
    ) -> Result<ApplyResult, BookError> {
        self.apply_delta_candidate(
            delta,
            session,
            now_monotonic_ns,
            Some(checksum),
            Some(previous_final_sequence),
            Some(source_latency_ms),
        )
    }

    /// Applies a validated normalized event when no source-exact checksum is
    /// attached. Checksum-bearing venue events must use the checked APIs so
    /// connector code can provide the original decimal strings.
    pub fn apply_event(&mut self, event: &EventEnvelope) -> Result<ApplyResult, BookError> {
        event.verify().map_err(|_| BookError::InvalidEvent)?;
        let metadata = event.metadata().as_unchecked();
        if metadata.source_checksum.is_some() {
            return Err(BookError::ChecksumSourceUnavailable);
        }
        let instrument_generation = metadata
            .instrument_id
            .as_ref()
            .ok_or(BookError::InvalidEvent)?
            .generation();
        if metadata.instrument_id.as_ref() != Some(self.config.instrument()) {
            return Err(BookError::WrongInstrument);
        }
        let session = BookSession {
            connection_epoch: metadata.connection_epoch,
            subscription_epoch: metadata.subscription_epoch,
            instrument_generation,
        };
        let source_latency_ms = metadata.exchange_timestamp.and_then(|exchange_timestamp| {
            metadata
                .receive_wall_timestamp
                .value()
                .checked_sub(exchange_timestamp.value())
                .and_then(|value| u64::try_from(value).ok())
                .map(|latency_ns| latency_ns / 1_000_000)
        });
        match event.payload().as_unchecked() {
            UncheckedEventPayload::BookSnapshot(snapshot) => self.apply_snapshot_candidate(
                snapshot.clone(),
                session,
                metadata.receive_monotonic_ns,
                None,
                source_latency_ms,
            ),
            UncheckedEventPayload::BookDelta(delta) => self.apply_delta_candidate(
                delta.clone(),
                session,
                metadata.receive_monotonic_ns,
                None,
                metadata.previous_sequence_number,
                source_latency_ms,
            ),
            _ => Err(BookError::InvalidEvent),
        }
    }

    pub fn snapshot(&self) -> Result<BookSnapshotView, BookError> {
        self.snapshot_at(self.updated_monotonic_ns)
    }

    pub fn snapshot_at(&self, now_monotonic_ns: u64) -> Result<BookSnapshotView, BookError> {
        if self.state != BookState::Synchronized {
            return Err(BookError::Untrusted);
        }
        if now_monotonic_ns < self.updated_monotonic_ns {
            return Err(BookError::MonotonicTimeRegression);
        }
        let book = self.book.as_ref().ok_or(BookError::Untrusted)?;
        let session = self.session.ok_or(BookError::Untrusted)?;
        let quality = BookQuality::score(
            self.state,
            book.sequence(),
            now_monotonic_ns.saturating_sub(self.updated_monotonic_ns) / 1_000_000,
            self.combined_checksum_status(),
            self.resync_count.count(now_monotonic_ns),
            self.missing_sequence_count.count(now_monotonic_ns),
            self.checksum_failure_count.count(now_monotonic_ns),
            self.crossed_state_count.count(now_monotonic_ns),
            self.source_latency.percentiles(now_monotonic_ns),
            book.levels(),
        );
        Ok(BookSnapshotView::new(
            book,
            session,
            self.updated_monotonic_ns,
            quality,
            self.l3
                .as_ref()
                .map_or_else(Vec::new, L3Book::sorted_orders),
        ))
    }

    pub fn apply_l3_snapshot(
        &mut self,
        _orders: Vec<L3Order>,
        session: BookSession,
        now_monotonic_ns: u64,
    ) -> Result<ApplyResult, BookError> {
        self.validate_current_session(session)?;
        self.observe_update_time(now_monotonic_ns)?;
        if self.state != BookState::Synchronized {
            return Err(BookError::Untrusted);
        }
        if self.config.max_l3_orders.is_none() {
            return Err(BookError::L3Disabled);
        }
        self.l3 = None;
        self.l3_checksum_status = ChecksumStatus::Pending;
        Err(BookError::ChecksumRequired)
    }

    pub fn apply_l3_snapshot_checked(
        &mut self,
        orders: Vec<L3Order>,
        session: BookSession,
        now_monotonic_ns: u64,
        source_latency_ms: u64,
        checksum: KrakenV2L3Checksum,
    ) -> Result<ApplyResult, BookError> {
        self.validate_current_session(session)?;
        self.observe_update_time(now_monotonic_ns)?;
        self.source_latency
            .record(now_monotonic_ns, source_latency_ms);
        if self.state != BookState::Synchronized {
            return Err(BookError::Untrusted);
        }
        let candidate = match L3Book::from_snapshot(orders, &self.config) {
            Ok(candidate) => candidate,
            Err(error) => {
                self.l3 = None;
                self.l3_checksum_status = ChecksumStatus::Mismatch;
                self.checksum_failure_count.record(now_monotonic_ns);
                return Err(error);
            }
        };
        if !self.validate_l3_checksum(&candidate, &checksum, now_monotonic_ns)? {
            return Ok(ApplyResult::ChecksumMismatch);
        }
        self.l3 = Some(candidate);
        self.updated_monotonic_ns = now_monotonic_ns;
        Ok(ApplyResult::Applied)
    }

    pub fn begin_resync(&mut self) -> Result<(), BookError> {
        if self.strategy != Some(SnapshotStrategy::ExternalBuffered)
            || self.state != BookState::Untrusted
        {
            return Err(BookError::InvalidTransition);
        }
        self.book = None;
        self.l3 = None;
        self.buffered.clear();
        self.buffered_level_updates = 0;
        self.updated_monotonic_ns = 0;
        self.checksum_status = match self.config.checksum_policy {
            ChecksumPolicy::Disabled => ChecksumStatus::NotSupported,
            ChecksumPolicy::Required => ChecksumStatus::Pending,
        };
        self.l3_checksum_status = if self.config.max_l3_orders.is_some() {
            ChecksumStatus::Pending
        } else {
            ChecksumStatus::NotSupported
        };
        self.resync_in_progress = true;
        self.state = BookState::Buffering;
        Ok(())
    }

    pub fn apply_l3_events(
        &mut self,
        _events: &[L3Event],
        session: BookSession,
        now_monotonic_ns: u64,
    ) -> Result<ApplyResult, BookError> {
        self.validate_current_session(session)?;
        self.observe_update_time(now_monotonic_ns)?;
        if self.state != BookState::Synchronized {
            return Err(BookError::Untrusted);
        }
        if self.config.max_l3_orders.is_none() {
            return Err(BookError::L3Disabled);
        }
        self.l3 = None;
        self.l3_checksum_status = ChecksumStatus::Pending;
        Err(BookError::ChecksumRequired)
    }

    pub fn apply_l3_events_checked(
        &mut self,
        events: &[L3Event],
        session: BookSession,
        now_monotonic_ns: u64,
        source_latency_ms: u64,
        checksum: KrakenV2L3Checksum,
    ) -> Result<ApplyResult, BookError> {
        self.validate_current_session(session)?;
        self.observe_update_time(now_monotonic_ns)?;
        self.source_latency
            .record(now_monotonic_ns, source_latency_ms);
        if self.state != BookState::Synchronized {
            return Err(BookError::Untrusted);
        }
        let mut book = self.l3.take().ok_or(BookError::L3SnapshotRequired)?;
        if let Err(error) = book.apply(events, &self.config) {
            self.l3_checksum_status = ChecksumStatus::Mismatch;
            self.checksum_failure_count.record(now_monotonic_ns);
            return Err(error);
        }
        if !self.validate_l3_checksum(&book, &checksum, now_monotonic_ns)? {
            return Ok(ApplyResult::ChecksumMismatch);
        }
        self.l3 = Some(book);
        self.updated_monotonic_ns = now_monotonic_ns;
        Ok(ApplyResult::Applied)
    }

    fn apply_snapshot_candidate(
        &mut self,
        snapshot: BookSnapshot,
        session: BookSession,
        now_monotonic_ns: u64,
        checksum: Option<KrakenV2Checksum>,
        source_latency_ms: Option<u64>,
    ) -> Result<ApplyResult, BookError> {
        self.validate_current_session(session)?;
        self.observe_update_time(now_monotonic_ns)?;
        if let Some(source_latency_ms) = source_latency_ms {
            self.source_latency
                .record(now_monotonic_ns, source_latency_ms);
        }
        if self.state == BookState::Disconnected {
            return Err(BookError::Untrusted);
        }
        let strategy = self.strategy.ok_or(BookError::InvalidTransition)?;
        if strategy == SnapshotStrategy::ExternalBuffered && self.state == BookState::Untrusted {
            return Err(BookError::InvalidTransition);
        }
        if strategy == SnapshotStrategy::ExternalBuffered && self.state == BookState::Synchronized {
            return Err(BookError::InvalidTransition);
        }
        let recovering = self.state == BookState::Untrusted || self.resync_in_progress;

        let mut candidate = match L2Book::from_snapshot(&snapshot, &self.config) {
            Ok(candidate) => candidate,
            Err(error) => {
                self.invalidate();
                return Err(error);
            }
        };
        if !self.validate_checksum(&candidate, checksum.as_ref(), now_monotonic_ns)? {
            return Ok(ApplyResult::ChecksumMismatch);
        }
        if strategy == SnapshotStrategy::StreamSnapshot {
            self.buffered.clear();
            self.buffered_level_updates = 0;
            self.install_trusted(candidate, now_monotonic_ns);
            if recovering {
                self.resync_count.record(now_monotonic_ns);
            }
            return Ok(ApplyResult::Applied);
        }

        self.state = BookState::Replaying;
        let mut latest_update = now_monotonic_ns;
        let mut first_replayed = true;
        while let Some(buffered) = self.buffered.pop_front() {
            self.buffered_level_updates = self
                .buffered_level_updates
                .saturating_sub(delta_level_updates(&buffered.delta));
            if buffered.session != session {
                self.invalidate();
                return Err(BookError::InvalidEpoch);
            }
            if buffered.delta.last_sequence <= candidate.sequence() {
                continue;
            }
            if !self.sequence_applies(
                &buffered.delta,
                candidate.sequence(),
                first_replayed,
                buffered.previous_final_sequence,
            )? {
                self.missing_sequence_count
                    .record(buffered.now_monotonic_ns);
                self.invalidate();
                return Ok(ApplyResult::GapDetected);
            }
            if let Err(error) = candidate.apply_delta(&buffered.delta, &self.config) {
                self.invalidate();
                return Err(error);
            }
            if !self.validate_checksum(
                &candidate,
                buffered.checksum.as_ref(),
                buffered.now_monotonic_ns,
            )? {
                return Ok(ApplyResult::ChecksumMismatch);
            }
            latest_update = latest_update.max(buffered.now_monotonic_ns);
            first_replayed = false;
        }
        self.install_trusted(candidate, latest_update);
        if recovering {
            self.resync_count.record(latest_update);
        }
        Ok(ApplyResult::Applied)
    }

    fn apply_delta_candidate(
        &mut self,
        delta: BookDelta,
        session: BookSession,
        now_monotonic_ns: u64,
        checksum: Option<KrakenV2Checksum>,
        previous_final_sequence: Option<u64>,
        source_latency_ms: Option<u64>,
    ) -> Result<ApplyResult, BookError> {
        self.validate_current_session(session)?;
        self.observe_update_time(now_monotonic_ns)?;
        if let Some(source_latency_ms) = source_latency_ms {
            self.source_latency
                .record(now_monotonic_ns, source_latency_ms);
        }
        if self.config.checksum_policy == ChecksumPolicy::Required && checksum.is_none() {
            self.invalidate();
            return Err(BookError::ChecksumRequired);
        }
        if delta.first_sequence == 0
            || delta.last_sequence == 0
            || delta.first_sequence > delta.last_sequence
        {
            self.invalidate();
            return Err(BookError::InvalidSequence);
        }
        if let Err(error) = L2Book::validate_delta(&delta, &self.config) {
            self.invalidate();
            return Err(error);
        }

        match self.state {
            BookState::Buffering | BookState::AwaitingSnapshot => {
                let level_updates = delta_level_updates(&delta);
                let Some(buffered_level_updates) =
                    self.buffered_level_updates.checked_add(level_updates)
                else {
                    self.invalidate();
                    return Err(BookError::BufferCapacity);
                };
                if self.buffered.len() >= self.config.max_buffered_deltas
                    || buffered_level_updates > self.config.max_buffered_level_updates
                {
                    self.invalidate();
                    return Err(BookError::BufferCapacity);
                }
                self.buffered.push_back(BufferedDelta {
                    delta,
                    session,
                    now_monotonic_ns,
                    checksum,
                    previous_final_sequence,
                });
                self.buffered_level_updates = buffered_level_updates;
                Ok(ApplyResult::SnapshotRequired)
            }
            BookState::Synchronized => {
                let current = self.book.as_ref().ok_or(BookError::Untrusted)?.sequence();
                if delta.last_sequence <= current {
                    return Ok(ApplyResult::Duplicate);
                }
                if !self.sequence_applies(&delta, current, false, previous_final_sequence)? {
                    self.missing_sequence_count.record(now_monotonic_ns);
                    self.invalidate();
                    return Ok(ApplyResult::GapDetected);
                }
                let mut candidate = self.book.take().ok_or(BookError::Untrusted)?;
                if let Err(error) = candidate.apply_delta(&delta, &self.config) {
                    self.invalidate();
                    return Err(error);
                }
                if !self.validate_checksum(&candidate, checksum.as_ref(), now_monotonic_ns)? {
                    self.book = Some(candidate);
                    return Ok(ApplyResult::ChecksumMismatch);
                }
                self.install_trusted(candidate, now_monotonic_ns);
                Ok(ApplyResult::Applied)
            }
            BookState::Disconnected | BookState::Replaying | BookState::Untrusted => {
                Ok(ApplyResult::SnapshotRequired)
            }
        }
    }

    fn sequence_applies(
        &mut self,
        delta: &BookDelta,
        current: u64,
        first_replayed: bool,
        previous_final_sequence: Option<u64>,
    ) -> Result<bool, BookError> {
        Ok(match self.config.sequence_policy {
            SequencePolicy::PreviousFinal if !first_replayed => {
                previous_final_sequence == Some(current)
            }
            SequencePolicy::ExactNext
            | SequencePolicy::RangeContainsNext
            | SequencePolicy::PreviousFinal => {
                let expected = current.checked_add(1).ok_or_else(|| {
                    self.invalidate();
                    BookError::SequenceOverflow
                })?;
                let range_contains_next =
                    delta.first_sequence <= expected && expected <= delta.last_sequence;
                match self.config.sequence_policy {
                    SequencePolicy::ExactNext => delta.first_sequence == expected,
                    SequencePolicy::RangeContainsNext | SequencePolicy::PreviousFinal => {
                        range_contains_next
                    }
                }
            }
        })
    }

    fn install_trusted(&mut self, book: L2Book, now_monotonic_ns: u64) {
        if book.classification() == BookClassification::Crossed {
            self.crossed_state_count.record(now_monotonic_ns);
        }
        self.book = Some(book);
        self.l3 = None;
        self.l3_checksum_status = if self.config.max_l3_orders.is_some() {
            ChecksumStatus::Pending
        } else {
            ChecksumStatus::NotSupported
        };
        self.updated_monotonic_ns = now_monotonic_ns;
        self.resync_in_progress = false;
        self.state = BookState::Synchronized;
    }

    fn validate_checksum(
        &mut self,
        candidate: &L2Book,
        checksum: Option<&KrakenV2Checksum>,
        now_monotonic_ns: u64,
    ) -> Result<bool, BookError> {
        let Some(checksum) = checksum else {
            if self.config.checksum_policy == ChecksumPolicy::Required {
                self.invalidate();
                return Err(BookError::ChecksumRequired);
            }
            self.checksum_status = ChecksumStatus::NotSupported;
            return Ok(true);
        };
        match checksum.verify_book(candidate) {
            Ok(true) => {
                self.checksum_status = ChecksumStatus::Valid;
                Ok(true)
            }
            Ok(false) => {
                self.checksum_status = ChecksumStatus::Mismatch;
                self.checksum_failure_count.record(now_monotonic_ns);
                self.invalidate();
                Ok(false)
            }
            Err(error) => {
                self.checksum_status = ChecksumStatus::Mismatch;
                self.checksum_failure_count.record(now_monotonic_ns);
                self.invalidate();
                Err(error)
            }
        }
    }

    fn validate_l3_checksum(
        &mut self,
        candidate: &L3Book,
        checksum: &KrakenV2L3Checksum,
        now_monotonic_ns: u64,
    ) -> Result<bool, BookError> {
        match checksum.verify_book(candidate) {
            Ok(true) => {
                self.l3_checksum_status = ChecksumStatus::Valid;
                Ok(true)
            }
            Ok(false) => {
                self.l3_checksum_status = ChecksumStatus::Mismatch;
                self.checksum_failure_count.record(now_monotonic_ns);
                self.l3 = None;
                Ok(false)
            }
            Err(error) => {
                self.l3_checksum_status = ChecksumStatus::Mismatch;
                self.checksum_failure_count.record(now_monotonic_ns);
                self.l3 = None;
                Err(error)
            }
        }
    }

    fn combined_checksum_status(&self) -> ChecksumStatus {
        match (self.checksum_status, self.l3_checksum_status) {
            (ChecksumStatus::Mismatch, _) | (_, ChecksumStatus::Mismatch) => {
                ChecksumStatus::Mismatch
            }
            (ChecksumStatus::Pending, _) | (_, ChecksumStatus::Pending) => ChecksumStatus::Pending,
            (ChecksumStatus::Valid, _) | (_, ChecksumStatus::Valid) => ChecksumStatus::Valid,
            _ => ChecksumStatus::NotSupported,
        }
    }

    fn invalidate(&mut self) {
        self.state = BookState::Untrusted;
        self.buffered.clear();
        self.buffered_level_updates = 0;
    }

    fn validate_new_session(&self, session: BookSession) -> Result<(), BookError> {
        if !session.is_valid() {
            return Err(BookError::InvalidEpoch);
        }
        if session.instrument_generation != self.config.instrument().generation() {
            return Err(BookError::StaleInstrumentGeneration);
        }
        if self
            .session
            .is_some_and(|previous| !session.is_strictly_after(previous))
        {
            return Err(BookError::InvalidEpoch);
        }
        Ok(())
    }

    fn validate_current_session(&self, session: BookSession) -> Result<(), BookError> {
        if !session.is_valid() {
            return Err(BookError::InvalidEpoch);
        }
        if session.instrument_generation != self.config.instrument().generation() {
            return Err(BookError::StaleInstrumentGeneration);
        }
        if self.session != Some(session) {
            return Err(BookError::InvalidEpoch);
        }
        Ok(())
    }

    fn observe_update_time(&mut self, now_monotonic_ns: u64) -> Result<(), BookError> {
        if now_monotonic_ns < self.last_observed_monotonic_ns {
            return Err(BookError::MonotonicTimeRegression);
        }
        self.last_observed_monotonic_ns = now_monotonic_ns;
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum BookError {
    #[error("invalid order-book configuration")]
    InvalidConfig,
    #[error("order book is not trusted")]
    Untrusted,
    #[error("invalid or stale connection/subscription epoch")]
    InvalidEpoch,
    #[error("stale instrument generation")]
    StaleInstrumentGeneration,
    #[error("event belongs to a different instrument shard")]
    WrongInstrument,
    #[error("invalid source sequence range")]
    InvalidSequence,
    #[error("source sequence overflow")]
    SequenceOverflow,
    #[error("order-book delta buffer capacity exceeded")]
    BufferCapacity,
    #[error("order-book level capacity exceeded")]
    Capacity,
    #[error("order-book level is not aligned to instrument metadata")]
    MisalignedLevel,
    #[error("order-book delta contains an ambiguous repeated price")]
    AmbiguousDelta,
    #[error("invalid order-book structure")]
    InvalidBook,
    #[error("checksum input is required by policy")]
    ChecksumRequired,
    #[error("checksum input is invalid")]
    InvalidChecksumInput,
    #[error("event is not a validated order-book event")]
    InvalidEvent,
    #[error("source-exact checksum decimals are unavailable")]
    ChecksumSourceUnavailable,
    #[error("monotonic update time regressed")]
    MonotonicTimeRegression,
    #[error("level-three order book is disabled")]
    L3Disabled,
    #[error("a level-three snapshot is required")]
    L3SnapshotRequired,
    #[error("invalid level-three order")]
    InvalidL3Order,
    #[error("ambiguous level-three order mutation")]
    AmbiguousL3Order,
    #[error("unknown level-three order")]
    UnknownL3Order,
    #[error("source sequence gap invalidated the order book")]
    SequenceGap,
    #[error("source checksum mismatch invalidated the order book")]
    ChecksumMismatch,
    #[error("a fresh source snapshot is required")]
    SnapshotRequired,
    #[error("invalid order-book synchronization transition")]
    InvalidTransition,
}

fn delta_level_updates(delta: &BookDelta) -> usize {
    delta.bids.len().saturating_add(delta.asks.len())
}
