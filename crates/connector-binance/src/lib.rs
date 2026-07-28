//! Fail-closed Binance native parsing plus the retained foundation fixture path.

mod book_sync;
mod capabilities;
mod metadata;
mod native;
mod normalizer;
mod parser;
mod session;

pub use book_sync::{BinanceBookSyncError, BinanceBookSynchronizer};
pub use capabilities::binance_capabilities;
pub use metadata::{
    BinanceInstrumentLifecycle, BinanceInstrumentLifecycleKind, BinanceInstrumentMetadata,
    ExchangeInfoReport, InstrumentMetadataBinding, MetadataError, parse_exchange_info_report,
};
pub use native::{
    AggregateTrade, BinanceInput, BinanceMarket, BinanceMessage, BookTicker, DepthSnapshot,
    DepthUpdate, DurableBinanceMessage, DurableDepthSnapshot, Liquidation,
    MAX_NATIVE_PAYLOAD_BYTES, MarkPrice, NativeBookLevel, NativeParseError, OpenInterest,
    parse_depth_snapshot, parse_durable_depth_snapshot, parse_durable_native_message,
    parse_native_message,
};
pub use normalizer::{
    NormalizationContext, NormalizationError, normalize_depth_snapshot, normalize_native_message,
};
pub use parser::{
    EXPECTED_GENERATION, EXPECTED_SOURCE, EXPECTED_SYMBOL, ParseError, parse_fixture_line,
};
pub use session::{
    BinanceConfig, BinanceConnector, BinanceSessionError, BinanceSessionRecord,
    BinanceSessionRoute, BinanceSessionSender, bounded_session_channel,
};
