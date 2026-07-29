//! Single-writer Deribit order-book continuity and reconnect recovery.

use std::collections::BTreeMap;

use event_envelope::{BookDelta, BookLevel, BookSnapshot};
use fixed_decimal::{Price, Quantity};
use orderbook::{
    ApplyResult, BookConfig, BookError, BookSession, BookSnapshotView, BookState, ChecksumPolicy,
    OrderBookEngine, SequencePolicy, SnapshotStrategy,
};
use thiserror::Error;

use crate::{BookAction, BookMessageKind, DeribitBookMessage, NativeBookLevel};

pub struct DeribitBookSynchronizer {
    engine: OrderBookEngine,
    instrument_name: String,
    bids: BTreeMap<Price, Quantity>,
    asks: BTreeMap<Price, Quantity>,
}

impl DeribitBookSynchronizer {
    pub fn try_new(config: BookConfig) -> Result<Self, DeribitBookSyncError> {
        if config.instrument.venue().as_str() != "deribit"
            || config.sequence_policy != SequencePolicy::PreviousFinal
            || config.checksum_policy != ChecksumPolicy::Disabled
            || config.max_l3_orders.is_some()
            || config.max_l3_levels_per_side.is_some()
        {
            return Err(DeribitBookSyncError::InvalidConfig);
        }
        Ok(Self {
            instrument_name: config.instrument.venue_symbol().to_owned(),
            engine: OrderBookEngine::new(config)?,
            bids: BTreeMap::new(),
            asks: BTreeMap::new(),
        })
    }

    pub fn start_session(&mut self, session: BookSession) -> Result<(), DeribitBookSyncError> {
        self.engine
            .start_session(session, SnapshotStrategy::StreamSnapshot)?;
        self.clear_local();
        Ok(())
    }

    pub fn disconnect(&mut self) {
        self.engine.disconnect();
        self.clear_local();
    }

    pub const fn state(&self) -> BookState {
        self.engine.state()
    }

    pub fn snapshot(&self) -> Result<BookSnapshotView, BookError> {
        self.engine.snapshot()
    }

    /// Applies one raw-WAL-acknowledged Deribit book notification.
    pub fn apply(
        &mut self,
        message: &DeribitBookMessage,
        now_monotonic_ns: u64,
    ) -> Result<ApplyResult, DeribitBookSyncError> {
        if message.instrument_name != self.instrument_name {
            return Err(DeribitBookSyncError::WrongInstrument);
        }
        let session = self
            .engine
            .current_session()
            .ok_or(DeribitBookSyncError::SessionNotStarted)?;
        match message.kind {
            BookMessageKind::Snapshot => self.apply_snapshot(message, session, now_monotonic_ns),
            BookMessageKind::Change => self.apply_change(message, session, now_monotonic_ns),
        }
    }

    fn apply_snapshot(
        &mut self,
        message: &DeribitBookMessage,
        session: BookSession,
        now_monotonic_ns: u64,
    ) -> Result<ApplyResult, DeribitBookSyncError> {
        if message.previous_change_id.is_some()
            || message
                .bids
                .iter()
                .chain(&message.asks)
                .any(|level| level.action != BookAction::New)
        {
            return Err(DeribitBookSyncError::InvalidTransition);
        }
        let candidate_bids = snapshot_map(&message.bids)?;
        let candidate_asks = snapshot_map(&message.asks)?;
        let result = self.engine.apply_snapshot(
            BookSnapshot {
                bids: event_levels(&message.bids),
                asks: event_levels(&message.asks),
                last_sequence: message.change_id,
            },
            session,
            now_monotonic_ns,
        )?;
        if result == ApplyResult::Applied {
            self.bids = candidate_bids;
            self.asks = candidate_asks;
        }
        Ok(result)
    }

    fn apply_change(
        &mut self,
        message: &DeribitBookMessage,
        session: BookSession,
        now_monotonic_ns: u64,
    ) -> Result<ApplyResult, DeribitBookSyncError> {
        let previous = message
            .previous_change_id
            .ok_or(DeribitBookSyncError::InvalidTransition)?;
        if self.engine.state() != BookState::Synchronized {
            return Ok(ApplyResult::SnapshotRequired);
        }
        let current = self.engine.snapshot()?.last_source_sequence();
        if message.change_id <= current || previous != current {
            let result = self.engine.apply_delta_with_previous(
                BookDelta {
                    bids: event_levels(&message.bids),
                    asks: event_levels(&message.asks),
                    first_sequence: message.change_id,
                    last_sequence: message.change_id,
                },
                session,
                now_monotonic_ns,
                previous,
            )?;
            if result == ApplyResult::GapDetected {
                self.clear_local();
            }
            return Ok(result);
        }
        let mut candidate_bids = self.bids.clone();
        let mut candidate_asks = self.asks.clone();
        apply_actions(&mut candidate_bids, &message.bids)?;
        apply_actions(&mut candidate_asks, &message.asks)?;
        let result = self.engine.apply_delta_with_previous(
            BookDelta {
                bids: event_levels(&message.bids),
                asks: event_levels(&message.asks),
                first_sequence: message.change_id,
                last_sequence: message.change_id,
            },
            session,
            now_monotonic_ns,
            previous,
        )?;
        match result {
            ApplyResult::Applied => {
                self.bids = candidate_bids;
                self.asks = candidate_asks;
            }
            ApplyResult::GapDetected => self.clear_local(),
            ApplyResult::Duplicate
            | ApplyResult::ChecksumMismatch
            | ApplyResult::SnapshotRequired => {}
        }
        Ok(result)
    }

    fn clear_local(&mut self) {
        self.bids.clear();
        self.asks.clear();
    }
}

fn snapshot_map(
    levels: &[NativeBookLevel],
) -> Result<BTreeMap<Price, Quantity>, DeribitBookSyncError> {
    let mut result = BTreeMap::new();
    for level in levels {
        if level.action != BookAction::New || result.insert(level.price, level.quantity).is_some() {
            return Err(DeribitBookSyncError::InvalidTransition);
        }
    }
    Ok(result)
}

fn apply_actions(
    levels: &mut BTreeMap<Price, Quantity>,
    updates: &[NativeBookLevel],
) -> Result<(), DeribitBookSyncError> {
    for update in updates {
        match update.action {
            BookAction::New if !levels.contains_key(&update.price) => {
                levels.insert(update.price, update.quantity);
            }
            BookAction::Change if levels.contains_key(&update.price) => {
                levels.insert(update.price, update.quantity);
            }
            BookAction::Delete if levels.remove(&update.price).is_some() => {}
            BookAction::New | BookAction::Change | BookAction::Delete => {
                return Err(DeribitBookSyncError::InvalidTransition);
            }
        }
    }
    Ok(())
}

fn event_levels(levels: &[NativeBookLevel]) -> Vec<BookLevel> {
    levels
        .iter()
        .map(|level| BookLevel {
            price: level.price,
            quantity: level.quantity,
            order_count: None,
        })
        .collect()
}

#[derive(Debug, Error)]
pub enum DeribitBookSyncError {
    #[error("Deribit order-book configuration is invalid")]
    InvalidConfig,
    #[error("Deribit order-book message targets the wrong instrument")]
    WrongInstrument,
    #[error("Deribit order-book session has not started")]
    SessionNotStarted,
    #[error("Deribit order-book action contradicts local state")]
    InvalidTransition,
    #[error(transparent)]
    Book(#[from] BookError),
}
