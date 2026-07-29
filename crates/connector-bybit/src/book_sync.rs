use domain::ProductType;
use event_envelope::{BookDelta, BookLevel, BookSnapshot};
use orderbook::{
    ApplyResult, BookConfig, BookError, BookSession, BookSnapshotView, BookState, ChecksumPolicy,
    OrderBookEngine, SequencePolicy, SnapshotStrategy,
};
use thiserror::Error;

use crate::{BookMessageKind, BybitMarket, OrderBookMessage};

/// Single-writer Bybit order-book synchronization over the shared engine.
pub struct BybitBookSynchronizer {
    engine: OrderBookEngine,
    market: BybitMarket,
    symbol: String,
    snapshot_reset_count: u64,
    last_cross_sequence: Option<u64>,
}

impl BybitBookSynchronizer {
    pub fn try_new(market: BybitMarket, config: BookConfig) -> Result<Self, BybitBookSyncError> {
        let expected_product = match market {
            BybitMarket::Spot => ProductType::Spot,
            BybitMarket::LinearPerpetual => ProductType::Perpetual,
        };
        if config.instrument.product_type() != expected_product
            || config.instrument.venue().as_str() != "bybit"
            || config.sequence_policy != SequencePolicy::ExactNext
            || config.checksum_policy != ChecksumPolicy::Disabled
        {
            return Err(BybitBookSyncError::InvalidConfig);
        }
        let symbol = config.instrument.venue_symbol().to_owned();
        Ok(Self {
            engine: OrderBookEngine::new(config)?,
            market,
            symbol,
            snapshot_reset_count: 0,
            last_cross_sequence: None,
        })
    }

    pub fn start_session(&mut self, session: BookSession) -> Result<(), BybitBookSyncError> {
        self.engine
            .start_session(session, SnapshotStrategy::StreamSnapshot)?;
        self.last_cross_sequence = None;
        Ok(())
    }

    pub fn disconnect(&mut self) {
        self.engine.disconnect();
        self.last_cross_sequence = None;
    }

    pub fn heartbeat_timeout(&mut self) {
        self.disconnect();
    }

    pub const fn state(&self) -> BookState {
        self.engine.state()
    }

    pub const fn current_session(&self) -> Option<BookSession> {
        self.engine.current_session()
    }

    pub const fn snapshot_reset_count(&self) -> u64 {
        self.snapshot_reset_count
    }

    pub const fn last_cross_sequence(&self) -> Option<u64> {
        self.last_cross_sequence
    }

    pub fn snapshot(&self) -> Result<BookSnapshotView, BookError> {
        self.engine.snapshot()
    }

    /// Applies a parsed message only after the caller has durably acknowledged
    /// its raw provider payload. Session integration owns that ordering.
    pub fn apply_message(
        &mut self,
        message: &OrderBookMessage,
        now_monotonic_ns: u64,
    ) -> Result<ApplyResult, BybitBookSyncError> {
        if message.market != self.market || message.symbol != self.symbol {
            return Err(BybitBookSyncError::WrongMessage);
        }
        if self
            .last_cross_sequence
            .is_some_and(|previous| message.cross_sequence < previous)
        {
            self.disconnect();
            return Err(BybitBookSyncError::CrossSequenceRegression);
        }
        let session = self
            .engine
            .current_session()
            .ok_or(BybitBookSyncError::SessionNotStarted)?;
        let result = match message.kind {
            BookMessageKind::Snapshot => self.engine.apply_snapshot(
                BookSnapshot {
                    bids: levels(&message.bids),
                    asks: levels(&message.asks),
                    last_sequence: message.update_id,
                },
                session,
                now_monotonic_ns,
            )?,
            BookMessageKind::Delta => self.engine.apply_delta(
                BookDelta {
                    bids: levels(&message.bids),
                    asks: levels(&message.asks),
                    first_sequence: message.update_id,
                    last_sequence: message.update_id,
                },
                session,
                now_monotonic_ns,
            )?,
        };
        if result == ApplyResult::Applied {
            self.last_cross_sequence = Some(message.cross_sequence);
            if message.kind == BookMessageKind::Snapshot {
                self.snapshot_reset_count = self.snapshot_reset_count.saturating_add(1);
            }
        }
        Ok(result)
    }
}

fn levels(levels: &[crate::NativeBookLevel]) -> Vec<BookLevel> {
    levels
        .iter()
        .map(|level| BookLevel {
            price: level.price,
            quantity: level.quantity,
            order_count: None,
        })
        .collect()
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum BybitBookSyncError {
    #[error("Bybit order-book configuration does not match market semantics")]
    InvalidConfig,
    #[error("Bybit order-book message does not match the configured market and instrument")]
    WrongMessage,
    #[error("Bybit cross sequence regressed")]
    CrossSequenceRegression,
    #[error("Bybit order-book session has not started")]
    SessionNotStarted,
    #[error("order-book engine rejected the update: {0}")]
    Book(#[from] BookError),
}
