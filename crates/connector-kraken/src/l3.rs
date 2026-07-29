//! Explicit capacity and allowlist policy for authenticated Kraken L3 data.

use std::collections::BTreeSet;
use thiserror::Error;

const MAX_TIER_A_SYMBOLS: usize = 200;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KrakenL3Policy {
    symbols: BTreeSet<String>,
    depth: usize,
    max_orders_per_symbol: usize,
}

impl KrakenL3Policy {
    pub fn disabled() -> Self {
        Self {
            symbols: BTreeSet::new(),
            depth: 10,
            max_orders_per_symbol: 1,
        }
    }

    pub fn try_new(
        symbols: impl IntoIterator<Item = String>,
        depth: usize,
        max_orders_per_symbol: usize,
    ) -> Result<Self, KrakenL3PolicyError> {
        if !matches!(depth, 10 | 100 | 1_000) || max_orders_per_symbol == 0 {
            return Err(KrakenL3PolicyError::InvalidCapacity);
        }
        let mut retained = BTreeSet::new();
        for symbol in symbols {
            if !valid_symbol(&symbol) || !retained.insert(symbol) {
                return Err(KrakenL3PolicyError::InvalidSymbol);
            }
        }
        if retained.is_empty() || retained.len() > MAX_TIER_A_SYMBOLS {
            return Err(KrakenL3PolicyError::InvalidCapacity);
        }
        Ok(Self {
            symbols: retained,
            depth,
            max_orders_per_symbol,
        })
    }

    pub fn is_enabled_for(&self, symbol: &str) -> bool {
        self.symbols.contains(symbol)
    }

    pub const fn depth(&self) -> usize {
        self.depth
    }

    pub const fn max_orders_per_symbol(&self) -> usize {
        self.max_orders_per_symbol
    }

    pub fn symbols(&self) -> impl Iterator<Item = &str> {
        self.symbols.iter().map(String::as_str)
    }
}

fn valid_symbol(symbol: &str) -> bool {
    !symbol.is_empty()
        && symbol.len() <= 64
        && symbol.trim() == symbol
        && symbol.is_ascii()
        && !symbol.chars().any(char::is_control)
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum KrakenL3PolicyError {
    #[error("Kraken L3 capacity must be positive and use a documented depth")]
    InvalidCapacity,
    #[error("Kraken L3 symbols must be unique bounded ASCII identifiers")]
    InvalidSymbol,
}
