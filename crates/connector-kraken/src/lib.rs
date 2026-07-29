//! Fail-closed Kraken Spot v2 and Futures market-data parsing and book integrity.

pub mod book_sync;
mod capabilities;
pub mod checksum;
pub mod l3;
pub mod parser;

pub use book_sync::{KrakenBookSyncError, KrakenBookSynchronizer};
pub use capabilities::kraken_capabilities as capabilities;
pub use checksum::{kraken_crc32, kraken_l3_crc32};
pub use l3::{KrakenL3Policy, KrakenL3PolicyError};
pub use parser::{
    AssetStatus, BookMessageKind, DurableKrakenMessage, ExactDecimal, FuturesBookMessage,
    FuturesTrade, KrakenInput, KrakenMessage, L3EventKind, MAX_NATIVE_PAYLOAD_BYTES,
    NativeBookLevel, NativeL3Order, NativeParseError, PairStatus, SpotAsset, SpotBookMessage,
    SpotInstrumentMessage, SpotL3Message, SpotPair, SpotTrade, SystemState,
    parse_durable_native_message, parse_native_message,
};
