//! Strict parser for the single offline Binance BTCUSDT fixture contract.

mod parser;

pub use parser::{
    EXPECTED_GENERATION, EXPECTED_SOURCE, EXPECTED_SYMBOL, ParseError, parse_fixture_line,
};
