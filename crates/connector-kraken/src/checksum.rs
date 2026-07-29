//! Kraken WebSocket v2 checksum construction from source-exact decimal text.

use orderbook::{
    BookError, ExactChecksumLevel, ExactL3ChecksumOrder, KrakenV2Checksum, KrakenV2L3Checksum,
};

/// Computes the documented Kraken v2 L2 CRC32 over asks then bids.
pub fn kraken_crc32(
    asks: Vec<ExactChecksumLevel>,
    bids: Vec<ExactChecksumLevel>,
) -> Result<u32, BookError> {
    Ok(KrakenV2Checksum::new(0, asks, bids)?.computed())
}

/// Computes the documented Kraken v2 L3 CRC32 including queue order.
pub fn kraken_l3_crc32(
    asks: Vec<ExactL3ChecksumOrder>,
    bids: Vec<ExactL3ChecksumOrder>,
) -> Result<u32, BookError> {
    Ok(KrakenV2L3Checksum::new(0, asks, bids)?.computed())
}
