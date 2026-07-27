use std::collections::BTreeMap;

use event_envelope::{BookDelta, BookLevel, BookSnapshot};
use fixed_decimal::{Price, Quantity};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BookHealth {
    Unsynchronized,
    Healthy,
    Gapped,
    ChecksumFailed,
    Stale,
    Quarantined,
}

#[derive(Clone)]
pub struct OrderBook {
    bids: BTreeMap<Price, Quantity>,
    asks: BTreeMap<Price, Quantity>,
    sequence: u64,
    health: BookHealth,
    updated_monotonic_ns: u64,
    max_levels: usize,
}

impl OrderBook {
    pub fn new(max_levels: usize) -> Self {
        Self {
            bids: BTreeMap::new(),
            asks: BTreeMap::new(),
            sequence: 0,
            health: BookHealth::Unsynchronized,
            updated_monotonic_ns: 0,
            max_levels: max_levels.max(1),
        }
    }

    pub fn apply_snapshot(
        &mut self,
        snapshot: &BookSnapshot,
        now_monotonic_ns: u64,
    ) -> Result<(), BookError> {
        if snapshot.last_sequence == 0
            || validate_levels(&snapshot.bids, &snapshot.asks, false, self.max_levels).is_err()
        {
            self.health = BookHealth::Quarantined;
            return Err(BookError::Invalid);
        }
        let bids = snapshot
            .bids
            .iter()
            .map(|level| (level.price, level.quantity))
            .collect::<BTreeMap<_, _>>();
        let asks = snapshot
            .asks
            .iter()
            .map(|level| (level.price, level.quantity))
            .collect::<BTreeMap<_, _>>();
        if validate_maps(&bids, &asks).is_err() {
            self.health = BookHealth::Quarantined;
            return Err(BookError::Invalid);
        }

        self.bids = bids;
        self.asks = asks;
        self.sequence = snapshot.last_sequence;
        self.health = BookHealth::Healthy;
        self.updated_monotonic_ns = now_monotonic_ns;
        Ok(())
    }

    pub fn apply_delta(
        &mut self,
        delta: &BookDelta,
        now_monotonic_ns: u64,
    ) -> Result<(), BookError> {
        if self.health != BookHealth::Healthy {
            return Err(BookError::Untrusted);
        }
        let next_sequence = self
            .sequence
            .checked_add(1)
            .ok_or(BookError::SequenceOverflow)?;
        if delta.first_sequence != next_sequence || delta.last_sequence < delta.first_sequence {
            self.health = BookHealth::Gapped;
            return Err(BookError::Gap);
        }
        if validate_levels(&delta.bids, &delta.asks, true, self.max_levels).is_err() {
            self.health = BookHealth::Quarantined;
            return Err(BookError::Invalid);
        }

        let mut bids = self.bids.clone();
        let mut asks = self.asks.clone();
        for level in &delta.bids {
            apply_level(&mut bids, level);
        }
        for level in &delta.asks {
            apply_level(&mut asks, level);
        }
        if bids.len().saturating_add(asks.len()) > self.max_levels {
            self.health = BookHealth::Quarantined;
            return Err(BookError::Capacity);
        }
        if validate_maps(&bids, &asks).is_err() {
            self.health = BookHealth::Quarantined;
            return Err(BookError::Invalid);
        }

        self.bids = bids;
        self.asks = asks;
        self.sequence = delta.last_sequence;
        self.updated_monotonic_ns = now_monotonic_ns;
        Ok(())
    }

    pub fn validate_checksum<F>(&mut self, expected: &str, checksum: F) -> Result<(), BookError>
    where
        F: FnOnce(&Self) -> String,
    {
        if checksum(self) == expected {
            Ok(())
        } else {
            self.health = BookHealth::ChecksumFailed;
            Err(BookError::Checksum)
        }
    }

    pub fn mark_stale(&mut self, now_monotonic_ns: u64, maximum_age_ns: u64) -> bool {
        if now_monotonic_ns.saturating_sub(self.updated_monotonic_ns) > maximum_age_ns {
            self.health = BookHealth::Stale;
            true
        } else {
            false
        }
    }

    pub fn reset(&mut self) {
        self.bids.clear();
        self.asks.clear();
        self.sequence = 0;
        self.health = BookHealth::Unsynchronized;
        self.updated_monotonic_ns = 0;
    }

    pub const fn health(&self) -> BookHealth {
        self.health
    }

    pub const fn sequence(&self) -> u64 {
        self.sequence
    }

    pub fn best_bid(&self) -> Option<Price> {
        self.bids.last_key_value().map(|(price, _)| *price)
    }

    pub fn best_ask(&self) -> Option<Price> {
        self.asks.first_key_value().map(|(price, _)| *price)
    }
}

fn apply_level(levels: &mut BTreeMap<Price, Quantity>, level: &BookLevel) {
    if level.quantity.value().is_zero() {
        levels.remove(&level.price);
    } else {
        levels.insert(level.price, level.quantity);
    }
}

fn validate_levels(
    bids: &[BookLevel],
    asks: &[BookLevel],
    allow_zero_quantity: bool,
    maximum_levels: usize,
) -> Result<(), BookError> {
    if bids.len().saturating_add(asks.len()) > maximum_levels
        || !allow_zero_quantity
            && bids
                .iter()
                .chain(asks)
                .any(|level| level.quantity.value().is_zero())
        || !bids.windows(2).all(|pair| pair[0].price > pair[1].price)
        || !asks.windows(2).all(|pair| pair[0].price < pair[1].price)
        || bids
            .first()
            .zip(asks.first())
            .is_some_and(|(bid, ask)| bid.price >= ask.price)
    {
        Err(BookError::Invalid)
    } else {
        Ok(())
    }
}

fn validate_maps(
    bids: &BTreeMap<Price, Quantity>,
    asks: &BTreeMap<Price, Quantity>,
) -> Result<(), BookError> {
    if bids.is_empty()
        || asks.is_empty()
        || bids
            .last_key_value()
            .zip(asks.first_key_value())
            .is_some_and(|((bid, _), (ask, _))| bid >= ask)
    {
        Err(BookError::Invalid)
    } else {
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum BookError {
    #[error("order book is not trusted")]
    Untrusted,
    #[error("order book sequence gap")]
    Gap,
    #[error("order book sequence overflow")]
    SequenceOverflow,
    #[error("order book checksum mismatch")]
    Checksum,
    #[error("order book capacity exceeded")]
    Capacity,
    #[error("invalid order book")]
    Invalid,
}
