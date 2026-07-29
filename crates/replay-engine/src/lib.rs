//! Deterministic verified-WAL replay through production connector paths.

mod config;
mod runner;

pub use config::{ExpectedReplayDigest, ReplayConfig, ReplayRecordKind};
pub use runner::{ReplayDigest, ReplayRunner};

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ReplayError {
    #[error("replay manifest is invalid: {0}")]
    InvalidManifest(&'static str),
    #[error("replay path is outside the approved workspace: {0}")]
    UnapprovedPath(&'static str),
    #[error("replay input hash mismatch")]
    InputHashMismatch,
    #[error("replay I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("replay manifest parsing failed: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("replay WAL failed: {0}")]
    Wal(#[from] raw_wal::manager::ManagerError),
    #[error("replay WAL prologue failed: {0}")]
    WalPrologue(#[from] raw_wal::prologue::PrologueError),
    #[error("recovered WAL proof failed: {0}")]
    RawProof(#[from] connector_core::RawCaptureError),
    #[error("Binance durable parsing failed: {0}")]
    BinanceParse(#[from] connector_binance::NativeParseError),
    #[error("Binance normalization failed: {0}")]
    BinanceNormalization(#[from] connector_binance::NormalizationError),
    #[error("normalized output validation failed: {0}")]
    NormalizedOutput(#[from] connector_core::ChannelError),
    #[error("order-book replay failed: {0}")]
    BinanceBook(#[from] connector_binance::BinanceBookSyncError),
    #[error("order-book snapshot failed: {0}")]
    Book(#[from] orderbook::BookError),
    #[error("instrument domain construction failed: {0}")]
    Domain(#[from] domain::DomainError),
    #[error("instrument registry failed: {0}")]
    Registry(#[from] instrument_registry::RegistryError),
    #[error("instrument catalog resolution failed: {0}")]
    Resolve(#[from] instrument_registry::ResolveError),
    #[error("fixed-decimal construction failed: {0}")]
    Decimal(#[from] fixed_decimal::DecimalError),
    #[error("event serialization failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("replay arithmetic exhausted")]
    Arithmetic,
    #[error("recovered WAL record does not match the manifest")]
    RecordMismatch,
    #[error("replay did not produce a trusted book")]
    MissingBook,
}
