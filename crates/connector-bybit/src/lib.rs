//! Fail-closed Bybit V5 native market-data parsing and synchronization.

pub mod book_sync;
mod capabilities;
mod metadata;
mod normalizer;
pub mod parser;

pub use book_sync::{BybitBookSyncError, BybitBookSynchronizer};
pub use capabilities::bybit_capabilities as capabilities;
pub use metadata::{
    BybitInstrumentLifecycle, BybitInstrumentMetadata, InstrumentsInfoReport, MetadataError,
    parse_instruments_info,
};
pub use normalizer::{NormalizationContext, NormalizationError, normalize_native_message};
pub use parser::{
    AllLiquidation, BookMessageKind, BybitInput, BybitMarket, BybitMessage, DurableBybitMessage,
    LinearTicker, MAX_NATIVE_PAYLOAD_BYTES, NativeBookLevel, NativeParseError, OrderBookMessage,
    PublicTrade, parse_durable_native_message, parse_native_message,
};
