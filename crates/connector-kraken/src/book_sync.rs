//! Single-writer Kraken Spot v2 order-book synchronization.

use event_envelope::{BookDelta, BookLevel, BookSnapshot};
use fixed_decimal::Price;
use orderbook::{
    ApplyResult, BookConfig, BookError, BookSession, BookSnapshotView, BookState, ChecksumPolicy,
    ExactChecksumLevel, ExactL3ChecksumOrder, KrakenV2Checksum, KrakenV2L3Checksum, L3Event,
    L3Order, L3OrderId, L3Side, OrderBookEngine, SequencePolicy, SnapshotStrategy,
};
use std::collections::BTreeMap;
use thiserror::Error;

use crate::{
    BookMessageKind, KrakenL3Policy, L3EventKind, NativeBookLevel, NativeL3Order, SpotBookMessage,
    SpotL3Message,
};

#[derive(Clone)]
struct ExactL3Entry {
    order: L3Order,
    price_text: String,
    quantity_text: String,
}

struct L3Candidate {
    entries: BTreeMap<L3OrderId, ExactL3Entry>,
    events: Vec<L3Event>,
    next_queue_order: u64,
}

pub struct KrakenBookSynchronizer {
    engine: OrderBookEngine,
    symbol: String,
    exact_bids: BTreeMap<Price, NativeBookLevel>,
    exact_asks: BTreeMap<Price, NativeBookLevel>,
    local_revision: u64,
    max_depth: usize,
    l3_policy: KrakenL3Policy,
    exact_l3: BTreeMap<L3OrderId, ExactL3Entry>,
    next_queue_order: u64,
}

impl KrakenBookSynchronizer {
    pub fn try_new(config: BookConfig) -> Result<Self, KrakenBookSyncError> {
        Self::try_new_with_l3(config, KrakenL3Policy::disabled())
    }

    pub fn try_new_with_l3(
        config: BookConfig,
        l3_policy: KrakenL3Policy,
    ) -> Result<Self, KrakenBookSyncError> {
        if config.instrument.venue().as_str() != "kraken"
            || config.sequence_policy != SequencePolicy::ExactNext
            || config.checksum_policy != ChecksumPolicy::Required
            || l3_policy.symbols().next().is_some() != config.max_l3_orders.is_some()
            || config.max_l3_orders.is_some_and(|maximum| {
                maximum != l3_policy.max_orders_per_symbol()
                    || config.max_l3_levels_per_side != Some(l3_policy.depth())
            })
        {
            return Err(KrakenBookSyncError::InvalidConfig);
        }
        let symbol = config.instrument.venue_symbol().to_owned();
        let max_depth = config.max_levels_per_side;
        Ok(Self {
            engine: OrderBookEngine::new(config)?,
            symbol,
            exact_bids: BTreeMap::new(),
            exact_asks: BTreeMap::new(),
            local_revision: 0,
            max_depth,
            l3_policy,
            exact_l3: BTreeMap::new(),
            next_queue_order: 0,
        })
    }

    pub fn start_session(&mut self, session: BookSession) -> Result<(), KrakenBookSyncError> {
        self.engine
            .start_session(session, SnapshotStrategy::StreamSnapshot)?;
        self.clear_exact();
        Ok(())
    }

    pub fn disconnect(&mut self) {
        self.engine.disconnect();
        self.clear_exact();
    }

    pub const fn state(&self) -> BookState {
        self.engine.state()
    }

    pub fn snapshot(&self) -> Result<BookSnapshotView, BookError> {
        self.engine.snapshot()
    }

    /// Applies one raw-WAL-acknowledged Spot v2 message atomically.
    pub fn apply_spot(
        &mut self,
        message: &SpotBookMessage,
        now_monotonic_ns: u64,
        source_latency_ms: u64,
    ) -> Result<ApplyResult, KrakenBookSyncError> {
        if canonical_spot_symbol(&message.symbol) != self.symbol {
            return Err(KrakenBookSyncError::WrongMessage);
        }
        let session = self
            .engine
            .current_session()
            .ok_or(KrakenBookSyncError::SessionNotStarted)?;
        let next_revision = self
            .local_revision
            .checked_add(1)
            .ok_or(KrakenBookSyncError::RevisionOverflow)?;
        let (mut candidate_bids, mut candidate_asks) = match message.kind {
            BookMessageKind::Snapshot => (BTreeMap::new(), BTreeMap::new()),
            BookMessageKind::Update => {
                if self.local_revision == 0 {
                    return Ok(ApplyResult::SnapshotRequired);
                }
                (self.exact_bids.clone(), self.exact_asks.clone())
            }
        };
        apply_levels(&mut candidate_bids, &message.bids);
        apply_levels(&mut candidate_asks, &message.asks);
        truncate_bids(&mut candidate_bids, self.max_depth);
        truncate_asks(&mut candidate_asks, self.max_depth);
        let checksum = checksum_for(message.checksum, &candidate_asks, &candidate_bids)?;
        let result = match message.kind {
            BookMessageKind::Snapshot => self.engine.apply_snapshot_checked(
                BookSnapshot {
                    bids: descending_levels(&candidate_bids),
                    asks: ascending_levels(&candidate_asks),
                    last_sequence: next_revision,
                },
                session,
                now_monotonic_ns,
                source_latency_ms,
                checksum,
            )?,
            BookMessageKind::Update => self.engine.apply_delta_checked(
                BookDelta {
                    bids: collapsed_book_levels(&message.bids),
                    asks: collapsed_book_levels(&message.asks),
                    first_sequence: next_revision,
                    last_sequence: next_revision,
                },
                session,
                now_monotonic_ns,
                source_latency_ms,
                checksum,
            )?,
        };
        if result == ApplyResult::Applied {
            self.exact_bids = candidate_bids;
            self.exact_asks = candidate_asks;
            self.local_revision = next_revision;
        } else if result == ApplyResult::ChecksumMismatch {
            self.clear_exact();
        }
        Ok(result)
    }

    /// Applies one raw-WAL-acknowledged, explicitly allowlisted Spot L3 message.
    pub fn apply_level3(
        &mut self,
        message: &SpotL3Message,
        now_monotonic_ns: u64,
        source_latency_ms: u64,
    ) -> Result<ApplyResult, KrakenBookSyncError> {
        if canonical_spot_symbol(&message.symbol) != self.symbol
            || !self.l3_policy.is_enabled_for(&message.symbol)
        {
            return Err(KrakenBookSyncError::L3NotAllowed);
        }
        if self.engine.state() != BookState::Synchronized {
            return Err(KrakenBookSyncError::L2NotSynchronized);
        }
        let session = self
            .engine
            .current_session()
            .ok_or(KrakenBookSyncError::SessionNotStarted)?;
        let L3Candidate {
            mut entries,
            events,
            next_queue_order,
        } = match message.kind {
            BookMessageKind::Snapshot => self.l3_snapshot_entries(message)?,
            BookMessageKind::Update => self.l3_update_entries(message)?,
        };
        truncate_l3(&mut entries, self.l3_policy.depth());
        if entries.len() > self.l3_policy.max_orders_per_symbol() {
            return Err(KrakenBookSyncError::L3Capacity);
        }
        let checksum = l3_checksum_for(message.checksum, &entries)?;
        let result = match message.kind {
            BookMessageKind::Snapshot => self.engine.apply_l3_snapshot_checked(
                entries.values().map(|entry| entry.order.clone()).collect(),
                session,
                now_monotonic_ns,
                source_latency_ms,
                checksum,
            )?,
            BookMessageKind::Update => self.engine.apply_l3_events_checked(
                &events,
                session,
                now_monotonic_ns,
                source_latency_ms,
                checksum,
            )?,
        };
        if result == ApplyResult::Applied {
            self.exact_l3 = entries;
            self.next_queue_order = next_queue_order;
        } else if result == ApplyResult::ChecksumMismatch {
            self.exact_l3.clear();
            self.next_queue_order = 0;
        }
        Ok(result)
    }

    fn l3_snapshot_entries(
        &self,
        message: &SpotL3Message,
    ) -> Result<L3Candidate, KrakenBookSyncError> {
        let mut entries = BTreeMap::new();
        let mut next_queue_order = 0;
        for (side, orders) in [
            (L3Side::Bid, message.bids.as_slice()),
            (L3Side::Ask, message.asks.as_slice()),
        ] {
            for source in orders {
                let entry = new_l3_entry(source, side, &mut next_queue_order)?;
                if entries.insert(entry.order.id().clone(), entry).is_some() {
                    return Err(KrakenBookSyncError::AmbiguousL3Order);
                }
            }
        }
        Ok(L3Candidate {
            entries,
            events: Vec::new(),
            next_queue_order,
        })
    }

    fn l3_update_entries(
        &self,
        message: &SpotL3Message,
    ) -> Result<L3Candidate, KrakenBookSyncError> {
        if self.exact_l3.is_empty() {
            return Err(KrakenBookSyncError::L3SnapshotRequired);
        }
        let mut candidate = self.exact_l3.clone();
        let mut events = Vec::new();
        let mut touched = std::collections::BTreeSet::new();
        let mut next_queue_order = self.next_queue_order;
        for (side, orders) in [
            (L3Side::Bid, message.bids.as_slice()),
            (L3Side::Ask, message.asks.as_slice()),
        ] {
            for source in orders {
                let id = L3OrderId::new(&source.order_id)?;
                if !touched.insert(id.clone()) {
                    return Err(KrakenBookSyncError::AmbiguousL3Order);
                }
                match source.event {
                    Some(L3EventKind::Add) => {
                        if candidate.contains_key(&id) {
                            return Err(KrakenBookSyncError::AmbiguousL3Order);
                        }
                        let entry = new_l3_entry(source, side, &mut next_queue_order)?;
                        events.push(L3Event::Add(entry.order.clone()));
                        candidate.insert(id, entry);
                    }
                    Some(L3EventKind::Modify) => {
                        let entry = candidate
                            .get_mut(&id)
                            .ok_or(KrakenBookSyncError::UnknownL3Order)?;
                        if entry.order.side() != side || entry.order.price() != source.price {
                            return Err(KrakenBookSyncError::AmbiguousL3Order);
                        }
                        events.push(L3Event::Modify {
                            id,
                            quantity: source.quantity,
                        });
                        entry.order = L3Order::new(
                            entry.order.id().clone(),
                            side,
                            source.price,
                            source.quantity,
                            entry.order.priority_ns(),
                            entry.order.queue_order(),
                        )?;
                        entry.quantity_text.clone_from(&source.quantity_text);
                    }
                    Some(L3EventKind::Delete) => {
                        if candidate.remove(&id).is_none() {
                            return Err(KrakenBookSyncError::UnknownL3Order);
                        }
                        events.push(L3Event::Delete { id });
                    }
                    None => return Err(KrakenBookSyncError::AmbiguousL3Order),
                }
            }
        }
        Ok(L3Candidate {
            entries: candidate,
            events,
            next_queue_order,
        })
    }

    fn clear_exact(&mut self) {
        self.exact_bids.clear();
        self.exact_asks.clear();
        self.exact_l3.clear();
        self.local_revision = 0;
        self.next_queue_order = 0;
    }
}

fn new_l3_entry(
    source: &NativeL3Order,
    side: L3Side,
    next_queue_order: &mut u64,
) -> Result<ExactL3Entry, KrakenBookSyncError> {
    *next_queue_order = next_queue_order
        .checked_add(1)
        .ok_or(KrakenBookSyncError::RevisionOverflow)?;
    Ok(ExactL3Entry {
        order: L3Order::new(
            L3OrderId::new(&source.order_id)?,
            side,
            source.price,
            source.quantity,
            *next_queue_order,
            *next_queue_order,
        )?,
        price_text: source.price_text.clone(),
        quantity_text: source.quantity_text.clone(),
    })
}

fn l3_checksum_for(
    expected: u32,
    entries: &BTreeMap<L3OrderId, ExactL3Entry>,
) -> Result<KrakenV2L3Checksum, BookError> {
    let asks = sorted_l3_side(entries, L3Side::Ask);
    let bids = sorted_l3_side(entries, L3Side::Bid);
    let asks = top_l3_levels(asks, L3Side::Ask)
        .into_iter()
        .map(exact_l3_order)
        .collect::<Result<Vec<_>, _>>()?;
    let bids = top_l3_levels(bids, L3Side::Bid)
        .into_iter()
        .map(exact_l3_order)
        .collect::<Result<Vec<_>, _>>()?;
    KrakenV2L3Checksum::new(expected, asks, bids)
}

fn sorted_l3_side(entries: &BTreeMap<L3OrderId, ExactL3Entry>, side: L3Side) -> Vec<&ExactL3Entry> {
    let mut retained = entries
        .values()
        .filter(|entry| entry.order.side() == side)
        .collect::<Vec<_>>();
    retained.sort_by(|left, right| {
        let price_order = match side {
            L3Side::Ask => left.order.price().cmp(&right.order.price()),
            L3Side::Bid => right.order.price().cmp(&left.order.price()),
        };
        price_order.then_with(|| {
            (left.order.priority_ns(), left.order.queue_order())
                .cmp(&(right.order.priority_ns(), right.order.queue_order()))
        })
    });
    retained
}

fn top_l3_levels(entries: Vec<&ExactL3Entry>, _side: L3Side) -> Vec<&ExactL3Entry> {
    let mut levels = 0_usize;
    let mut last_price = None;
    entries
        .into_iter()
        .take_while(|entry| {
            if last_price != Some(entry.order.price()) {
                levels = levels.saturating_add(1);
                last_price = Some(entry.order.price());
            }
            levels <= 10
        })
        .collect()
}

fn exact_l3_order(entry: &ExactL3Entry) -> Result<ExactL3ChecksumOrder, BookError> {
    ExactL3ChecksumOrder::new(entry.order.clone(), &entry.price_text, &entry.quantity_text)
}

fn truncate_l3(entries: &mut BTreeMap<L3OrderId, ExactL3Entry>, maximum_levels: usize) {
    for side in [L3Side::Bid, L3Side::Ask] {
        let retained = sorted_l3_side(entries, side);
        let mut levels = 0_usize;
        let mut last_price = None;
        let remove = retained
            .into_iter()
            .filter_map(|entry| {
                if last_price != Some(entry.order.price()) {
                    levels = levels.saturating_add(1);
                    last_price = Some(entry.order.price());
                }
                (levels > maximum_levels).then(|| entry.order.id().clone())
            })
            .collect::<Vec<_>>();
        for id in remove {
            entries.remove(&id);
        }
    }
}

fn canonical_spot_symbol(symbol: &str) -> String {
    symbol
        .chars()
        .filter(|character| *character != '/')
        .collect()
}

fn apply_levels(side: &mut BTreeMap<Price, NativeBookLevel>, levels: &[NativeBookLevel]) {
    for level in levels {
        if level.quantity.value().is_zero() {
            side.remove(&level.price);
        } else {
            side.insert(level.price, level.clone());
        }
    }
}

fn truncate_bids(side: &mut BTreeMap<Price, NativeBookLevel>, maximum: usize) {
    while side.len() > maximum {
        let Some(price) = side.first_key_value().map(|(price, _)| *price) else {
            break;
        };
        side.remove(&price);
    }
}

fn truncate_asks(side: &mut BTreeMap<Price, NativeBookLevel>, maximum: usize) {
    while side.len() > maximum {
        let Some(price) = side.last_key_value().map(|(price, _)| *price) else {
            break;
        };
        side.remove(&price);
    }
}

fn checksum_for(
    expected: u32,
    asks: &BTreeMap<Price, NativeBookLevel>,
    bids: &BTreeMap<Price, NativeBookLevel>,
) -> Result<KrakenV2Checksum, BookError> {
    let asks = asks
        .values()
        .take(10)
        .map(exact_level)
        .collect::<Result<Vec<_>, _>>()?;
    let bids = bids
        .values()
        .rev()
        .take(10)
        .map(exact_level)
        .collect::<Result<Vec<_>, _>>()?;
    KrakenV2Checksum::new(expected, asks, bids)
}

fn exact_level(level: &NativeBookLevel) -> Result<ExactChecksumLevel, BookError> {
    ExactChecksumLevel::new(&level.price_text, &level.quantity_text)
}

fn collapsed_book_levels(levels: &[NativeBookLevel]) -> Vec<BookLevel> {
    let mut final_by_price = BTreeMap::new();
    for level in levels {
        final_by_price.insert(
            level.price,
            BookLevel {
                price: level.price,
                quantity: level.quantity,
                order_count: None,
            },
        );
    }
    final_by_price.into_values().collect()
}

fn descending_levels(levels: &BTreeMap<Price, NativeBookLevel>) -> Vec<BookLevel> {
    levels
        .values()
        .rev()
        .map(|level| BookLevel {
            price: level.price,
            quantity: level.quantity,
            order_count: None,
        })
        .collect()
}

fn ascending_levels(levels: &BTreeMap<Price, NativeBookLevel>) -> Vec<BookLevel> {
    levels
        .values()
        .map(|level| BookLevel {
            price: level.price,
            quantity: level.quantity,
            order_count: None,
        })
        .collect()
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum KrakenBookSyncError {
    #[error("Kraken order-book configuration does not match Spot v2 checksum semantics")]
    InvalidConfig,
    #[error("Kraken order-book message does not match the configured instrument")]
    WrongMessage,
    #[error("Kraken order-book session has not started")]
    SessionNotStarted,
    #[error("Kraken local book revision overflowed")]
    RevisionOverflow,
    #[error("Kraken L3 is not enabled for this Tier A symbol")]
    L3NotAllowed,
    #[error("Kraken L2 must be synchronized before L3 can be applied")]
    L2NotSynchronized,
    #[error("Kraken L3 requires a trusted snapshot before updates")]
    L3SnapshotRequired,
    #[error("Kraken L3 message references an unknown source-scoped order")]
    UnknownL3Order,
    #[error("Kraken L3 message has ambiguous order lifecycle")]
    AmbiguousL3Order,
    #[error("Kraken L3 capacity was exceeded")]
    L3Capacity,
    #[error("order-book engine rejected the update: {0}")]
    Book(#[from] BookError),
}
