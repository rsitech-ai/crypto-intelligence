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
pub use connector_core::{
    DerivativeNormalizationReceipt as BinanceDerivativeNormalizationReceipt,
    DerivativeStream as BinanceDerivativeStream,
};
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
    BinanceNormalizedEvent, BinanceTradeNormalizationReceipt, NormalizationContext,
    NormalizationError, normalize_depth_snapshot, normalize_derivative_with_receipts,
    normalize_native_message, normalize_native_with_authority, normalize_trade_with_receipt,
};
pub use parser::{
    EXPECTED_GENERATION, EXPECTED_SOURCE, EXPECTED_SYMBOL, ParseError, parse_fixture_line,
};
pub use session::{
    BinanceConfig, BinanceConnector, BinanceSessionError, BinanceSessionRecord,
    BinanceSessionRoute, BinanceSessionSender, BinanceStreamContract, bounded_session_channel,
};
