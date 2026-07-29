//! Fail-closed Deribit futures, perpetuals, and options market-data adapter.

pub mod book_sync;
mod capabilities;
pub mod options;
pub mod parser;

pub use book_sync::{DeribitBookSyncError, DeribitBookSynchronizer};
pub use capabilities::deribit_capabilities as capabilities;
pub use options::{
    DeribitInstrumentBinding, InstrumentNormalizationError, NormalizedOptionTicker,
    SourceOptionGreeks, normalize_option_ticker,
};
pub use parser::{
    BookAction, BookMessageKind, DeribitBookMessage, DeribitDeliveryPrice, DeribitInput,
    DeribitInstrument, DeribitInstrumentKind, DeribitInstrumentType, DeribitLifecycle,
    DeribitMessage, DeribitPerpetualInterest, DeribitTicker, DeribitTickerStats, DeribitTrade,
    DurableDeribitMessage, ExactDecimal, MAX_NATIVE_PAYLOAD_BYTES, NativeBookLevel,
    NativeParseError, OptionKind, parse_durable_native_message, parse_native_message,
};
