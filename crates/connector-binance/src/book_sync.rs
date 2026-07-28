use connector_core::NormalizedOutput;
use domain::ProductType;
use orderbook::{
    ApplyResult, BookConfig, BookError, BookSession, BookSnapshotView, BookState, ChecksumPolicy,
    OrderBookEngine, SequencePolicy, SnapshotStrategy,
};
use thiserror::Error;

use crate::BinanceMarket;

pub struct BinanceBookSynchronizer {
    engine: OrderBookEngine,
    resnapshot_count: u64,
}

impl BinanceBookSynchronizer {
    pub fn try_new(
        market: BinanceMarket,
        config: BookConfig,
    ) -> Result<Self, BinanceBookSyncError> {
        let expected_product = match market {
            BinanceMarket::Spot => ProductType::Spot,
            BinanceMarket::UsdMarginedPerpetual => ProductType::Perpetual,
        };
        let expected_policy = match market {
            BinanceMarket::Spot => SequencePolicy::RangeContainsNext,
            BinanceMarket::UsdMarginedPerpetual => SequencePolicy::PreviousFinal,
        };
        if config.instrument.product_type() != expected_product
            || config.instrument.venue().as_str() != "binance"
            || config.sequence_policy != expected_policy
            || config.checksum_policy != ChecksumPolicy::Disabled
        {
            return Err(BinanceBookSyncError::InvalidConfig);
        }
        let engine = OrderBookEngine::new(config)?;
        Ok(Self {
            engine,
            resnapshot_count: 0,
        })
    }

    pub fn start_session(&mut self, session: BookSession) -> Result<(), BinanceBookSyncError> {
        self.engine
            .start_session(session, SnapshotStrategy::ExternalBuffered)?;
        Ok(())
    }

    pub fn renew_session(&mut self, session: BookSession) -> Result<(), BinanceBookSyncError> {
        self.engine
            .start_session(session, SnapshotStrategy::ExternalBuffered)?;
        self.resnapshot_count = self.resnapshot_count.saturating_add(1);
        Ok(())
    }

    pub fn disconnect(&mut self) {
        self.engine.disconnect();
    }

    pub fn heartbeat_timeout(&mut self) {
        self.engine.disconnect();
        self.resnapshot_count = self.resnapshot_count.saturating_add(1);
    }

    pub const fn state(&self) -> BookState {
        self.engine.state()
    }

    pub const fn current_session(&self) -> Option<BookSession> {
        self.engine.current_session()
    }

    pub const fn resnapshot_count(&self) -> u64 {
        self.resnapshot_count
    }

    pub fn snapshot(&self) -> Result<BookSnapshotView, BookError> {
        self.engine.snapshot()
    }

    /// Applies only an event that has already passed raw WAL acknowledgement,
    /// durable parsing, normalization, and raw/event linkage validation.
    pub fn apply_output(
        &mut self,
        output: &NormalizedOutput,
    ) -> Result<ApplyResult, BinanceBookSyncError> {
        let outcome = match self.engine.apply_event(output.event()) {
            Ok(outcome) => outcome,
            Err(error) if invalidated_by(&error) => {
                self.begin_recovery()?;
                return Err(BinanceBookSyncError::Book(error));
            }
            Err(error) => return Err(BinanceBookSyncError::Book(error)),
        };
        if matches!(
            outcome,
            ApplyResult::GapDetected | ApplyResult::ChecksumMismatch
        ) {
            self.begin_recovery()?;
        }
        Ok(outcome)
    }

    fn begin_recovery(&mut self) -> Result<(), BinanceBookSyncError> {
        self.engine.begin_resync()?;
        self.resnapshot_count = self.resnapshot_count.saturating_add(1);
        Ok(())
    }
}

fn invalidated_by(error: &BookError) -> bool {
    matches!(
        error,
        BookError::InvalidSequence
            | BookError::SequenceOverflow
            | BookError::BufferCapacity
            | BookError::Capacity
            | BookError::MisalignedLevel
            | BookError::AmbiguousDelta
            | BookError::InvalidBook
            | BookError::ChecksumRequired
            | BookError::ChecksumMismatch
            | BookError::SequenceGap
            | BookError::SnapshotRequired
    )
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum BinanceBookSyncError {
    #[error("Binance order-book configuration does not match market semantics")]
    InvalidConfig,
    #[error("order-book engine rejected the update: {0}")]
    Book(#[from] BookError),
}
