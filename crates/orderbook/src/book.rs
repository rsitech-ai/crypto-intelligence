use crate::{BookClassification, BookConfig, BookError, BookQuality, BookSession};
use domain::InstrumentId;
use event_envelope::{BookDelta, BookLevel, BookSnapshot};
use fixed_decimal::{FixedDecimal, Price, Quantity};
use std::collections::{BTreeMap, BTreeSet};

const MAX_L3_ORDER_ID_BYTES: usize = 192;

#[derive(Clone, Debug, Eq, PartialEq)]
struct LevelValue {
    quantity: Quantity,
    order_count: Option<u32>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct L2Book {
    bids: BTreeMap<Price, LevelValue>,
    asks: BTreeMap<Price, LevelValue>,
    sequence: u64,
}

impl L2Book {
    pub(crate) fn from_snapshot(
        snapshot: &BookSnapshot,
        config: &BookConfig,
    ) -> Result<Self, BookError> {
        if snapshot.last_sequence == 0 {
            return Err(BookError::InvalidSequence);
        }
        if snapshot.bids.is_empty() && snapshot.asks.is_empty() {
            return Err(BookError::InvalidBook);
        }
        validate_levels(&snapshot.bids, false, config)?;
        validate_levels(&snapshot.asks, false, config)?;
        if snapshot.bids.len() > config.max_levels_per_side
            || snapshot.asks.len() > config.max_levels_per_side
            || !strictly_descending(&snapshot.bids)
            || !strictly_ascending(&snapshot.asks)
        {
            return Err(BookError::InvalidBook);
        }

        Ok(Self {
            bids: snapshot
                .bids
                .iter()
                .map(|level| (level.price, level_value(level)))
                .collect(),
            asks: snapshot
                .asks
                .iter()
                .map(|level| (level.price, level_value(level)))
                .collect(),
            sequence: snapshot.last_sequence,
        })
    }

    pub(crate) fn apply_delta(
        &mut self,
        delta: &BookDelta,
        config: &BookConfig,
    ) -> Result<(), BookError> {
        validate_levels(&delta.bids, true, config)?;
        validate_levels(&delta.asks, true, config)?;
        ensure_unique_prices(&delta.bids)?;
        ensure_unique_prices(&delta.asks)?;

        apply_side(&mut self.bids, &delta.bids);
        apply_side(&mut self.asks, &delta.asks);
        truncate_bids(&mut self.bids, config.max_levels_per_side);
        truncate_asks(&mut self.asks, config.max_levels_per_side);
        self.sequence = delta.last_sequence;
        Ok(())
    }

    pub(crate) fn validate_delta(delta: &BookDelta, config: &BookConfig) -> Result<(), BookError> {
        if delta.bids.is_empty() && delta.asks.is_empty() {
            return Err(BookError::InvalidBook);
        }
        validate_levels(&delta.bids, true, config)?;
        validate_levels(&delta.asks, true, config)?;
        ensure_unique_prices(&delta.bids)?;
        ensure_unique_prices(&delta.asks)
    }

    pub(crate) const fn sequence(&self) -> u64 {
        self.sequence
    }

    pub(crate) fn classification(&self) -> BookClassification {
        match (self.bids.last_key_value(), self.asks.first_key_value()) {
            (Some((bid, _)), Some((ask, _))) if bid < ask => BookClassification::Normal,
            (Some((bid, _)), Some((ask, _))) if bid == ask => BookClassification::Locked,
            (Some(_), Some(_)) => BookClassification::Crossed,
            _ => BookClassification::OneSided,
        }
    }

    pub(crate) fn levels(&self) -> usize {
        self.bids.len().saturating_add(self.asks.len())
    }

    pub(crate) fn bid_levels(&self) -> usize {
        self.bids.len()
    }

    pub(crate) fn ask_levels(&self) -> usize {
        self.asks.len()
    }

    pub(crate) fn top_bids(&self, depth: usize) -> impl Iterator<Item = (Price, Quantity)> + '_ {
        self.bids
            .iter()
            .rev()
            .take(depth)
            .map(|(price, value)| (*price, value.quantity))
    }

    pub(crate) fn top_asks(&self, depth: usize) -> impl Iterator<Item = (Price, Quantity)> + '_ {
        self.asks
            .iter()
            .take(depth)
            .map(|(price, value)| (*price, value.quantity))
    }

    pub(crate) fn bids_descending(&self) -> Vec<BookLevel> {
        self.bids
            .iter()
            .rev()
            .map(|(price, value)| BookLevel {
                price: *price,
                quantity: value.quantity,
                order_count: value.order_count,
            })
            .collect()
    }

    pub(crate) fn asks_ascending(&self) -> Vec<BookLevel> {
        self.asks
            .iter()
            .map(|(price, value)| BookLevel {
                price: *price,
                quantity: value.quantity,
                order_count: value.order_count,
            })
            .collect()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BookSnapshotView {
    instrument: InstrumentId,
    bids: Vec<BookLevel>,
    asks: Vec<BookLevel>,
    last_source_sequence: u64,
    session: BookSession,
    classification: BookClassification,
    updated_monotonic_ns: u64,
    quality: BookQuality,
    l3_orders: Vec<L3Order>,
}

impl BookSnapshotView {
    pub(crate) fn new(
        instrument: InstrumentId,
        book: &L2Book,
        session: BookSession,
        updated_monotonic_ns: u64,
        quality: BookQuality,
        l3_orders: Vec<L3Order>,
    ) -> Self {
        Self {
            instrument,
            bids: book.bids_descending(),
            asks: book.asks_ascending(),
            last_source_sequence: book.sequence(),
            session,
            classification: book.classification(),
            updated_monotonic_ns,
            quality,
            l3_orders,
        }
    }

    pub const fn instrument(&self) -> &InstrumentId {
        &self.instrument
    }

    pub fn bids(&self) -> &[BookLevel] {
        &self.bids
    }

    pub fn asks(&self) -> &[BookLevel] {
        &self.asks
    }

    pub fn best_bid(&self) -> Option<&BookLevel> {
        self.bids.first()
    }

    pub fn best_ask(&self) -> Option<&BookLevel> {
        self.asks.first()
    }

    pub const fn last_source_sequence(&self) -> u64 {
        self.last_source_sequence
    }

    pub const fn session(&self) -> BookSession {
        self.session
    }

    pub const fn classification(&self) -> BookClassification {
        self.classification
    }

    pub const fn updated_monotonic_ns(&self) -> u64 {
        self.updated_monotonic_ns
    }

    pub const fn quality(&self) -> &BookQuality {
        &self.quality
    }

    pub fn l3_orders(&self) -> &[L3Order] {
        &self.l3_orders
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum L3Side {
    Bid,
    Ask,
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct L3OrderId(String);

impl L3OrderId {
    pub fn new(value: impl Into<String>) -> Result<Self, BookError> {
        let value = value.into();
        if value.is_empty()
            || value.len() > MAX_L3_ORDER_ID_BYTES
            || value.trim() != value
            || !value.is_ascii()
            || value.chars().any(char::is_control)
        {
            return Err(BookError::InvalidL3Order);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct L3Order {
    id: L3OrderId,
    side: L3Side,
    price: Price,
    quantity: Quantity,
    priority_ns: u64,
    queue_order: u64,
}

impl L3Order {
    pub fn new(
        id: L3OrderId,
        side: L3Side,
        price: Price,
        quantity: Quantity,
        priority_ns: u64,
        queue_order: u64,
    ) -> Result<Self, BookError> {
        if quantity.value().is_zero() {
            return Err(BookError::InvalidL3Order);
        }
        Ok(Self {
            id,
            side,
            price,
            quantity,
            priority_ns,
            queue_order,
        })
    }

    pub fn id(&self) -> &L3OrderId {
        &self.id
    }

    pub const fn side(&self) -> L3Side {
        self.side
    }

    pub const fn price(&self) -> Price {
        self.price
    }

    pub const fn quantity(&self) -> Quantity {
        self.quantity
    }

    pub const fn priority_ns(&self) -> u64 {
        self.priority_ns
    }

    pub const fn queue_order(&self) -> u64 {
        self.queue_order
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum L3Event {
    Add(L3Order),
    Modify { id: L3OrderId, quantity: Quantity },
    Delete { id: L3OrderId },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct L3Book {
    orders: BTreeMap<L3OrderId, L3Order>,
    bids: BTreeMap<Price, BTreeMap<(u64, u64), L3OrderId>>,
    asks: BTreeMap<Price, BTreeMap<(u64, u64), L3OrderId>>,
}

impl L3Book {
    pub(crate) fn from_snapshot(
        orders: Vec<L3Order>,
        config: &BookConfig,
    ) -> Result<Self, BookError> {
        let maximum = config.max_l3_orders.ok_or(BookError::L3Disabled)?;
        let depth = config.max_l3_levels_per_side.ok_or(BookError::L3Disabled)?;
        if orders.len() > maximum {
            return Err(BookError::Capacity);
        }
        let mut book = Self {
            orders: BTreeMap::new(),
            bids: BTreeMap::new(),
            asks: BTreeMap::new(),
        };
        for order in orders {
            validate_l3_order(&order, config)?;
            book.insert(order)?;
        }
        book.truncate(depth);
        Ok(book)
    }

    pub(crate) fn apply(
        &mut self,
        events: &[L3Event],
        config: &BookConfig,
    ) -> Result<(), BookError> {
        let maximum = config.max_l3_orders.ok_or(BookError::L3Disabled)?;
        let depth = config.max_l3_levels_per_side.ok_or(BookError::L3Disabled)?;
        if events.len() > maximum {
            return Err(BookError::Capacity);
        }
        let mut touched = BTreeSet::new();
        let mut additions = 0_usize;
        let mut deletions = 0_usize;
        for event in events {
            let id = match event {
                L3Event::Add(order) => order.id(),
                L3Event::Modify { id, .. } | L3Event::Delete { id } => id,
            };
            if !touched.insert(id.clone()) {
                return Err(BookError::AmbiguousL3Order);
            }
            match event {
                L3Event::Add(order) => {
                    validate_l3_order(order, config)?;
                    if self.orders.contains_key(order.id()) {
                        return Err(BookError::AmbiguousL3Order);
                    }
                    additions = additions.saturating_add(1);
                }
                L3Event::Modify { id, quantity } => {
                    if quantity.value().is_zero()
                        || !decimal_is_multiple(quantity.value(), config.quantity_step.value())
                    {
                        return Err(BookError::InvalidL3Order);
                    }
                    if !self.orders.contains_key(id) {
                        return Err(BookError::UnknownL3Order);
                    }
                }
                L3Event::Delete { id } => {
                    if !self.orders.contains_key(id) {
                        return Err(BookError::UnknownL3Order);
                    }
                    deletions = deletions.saturating_add(1);
                }
            }
        }
        let final_count = self
            .orders
            .len()
            .checked_sub(deletions)
            .and_then(|count| count.checked_add(additions))
            .ok_or(BookError::Capacity)?;
        if final_count > maximum {
            return Err(BookError::Capacity);
        }

        for event in events {
            if let L3Event::Delete { id } = event {
                self.remove(id)?;
            }
        }
        for event in events {
            if let L3Event::Modify { id, quantity } = event {
                let order = self.orders.get_mut(id).ok_or(BookError::UnknownL3Order)?;
                order.quantity = *quantity;
            }
        }
        for event in events {
            if let L3Event::Add(order) = event {
                self.insert(order.clone())?;
            }
        }
        self.truncate(depth);
        Ok(())
    }

    pub(crate) fn sorted_orders(&self) -> Vec<L3Order> {
        let mut orders = Vec::with_capacity(self.orders.len());
        self.append_side(&mut orders, L3Side::Bid, usize::MAX);
        self.append_side(&mut orders, L3Side::Ask, usize::MAX);
        orders
    }

    pub(crate) fn checksum_orders(&self, side: L3Side) -> Vec<L3Order> {
        let mut orders = Vec::new();
        self.append_side(&mut orders, side, 10);
        orders
    }

    fn insert(&mut self, order: L3Order) -> Result<(), BookError> {
        if self.orders.contains_key(order.id()) {
            return Err(BookError::AmbiguousL3Order);
        }
        let queue = self.side_mut(order.side).entry(order.price).or_default();
        let priority = (order.priority_ns, order.queue_order);
        if queue.insert(priority, order.id.clone()).is_some() {
            return Err(BookError::AmbiguousL3Order);
        }
        self.orders.insert(order.id.clone(), order);
        Ok(())
    }

    fn remove(&mut self, id: &L3OrderId) -> Result<L3Order, BookError> {
        let order = self.orders.remove(id).ok_or(BookError::UnknownL3Order)?;
        let priority = (order.priority_ns, order.queue_order);
        let side = self.side_mut(order.side);
        let queue = side
            .get_mut(&order.price)
            .ok_or(BookError::InvalidL3Order)?;
        if queue.remove(&priority).as_ref() != Some(id) {
            return Err(BookError::InvalidL3Order);
        }
        if queue.is_empty() {
            side.remove(&order.price);
        }
        Ok(order)
    }

    fn truncate(&mut self, depth: usize) {
        while self.bids.len() > depth {
            let Some(price) = self.bids.first_key_value().map(|(price, _)| *price) else {
                break;
            };
            self.remove_level(L3Side::Bid, price);
        }
        while self.asks.len() > depth {
            let Some(price) = self.asks.last_key_value().map(|(price, _)| *price) else {
                break;
            };
            self.remove_level(L3Side::Ask, price);
        }
    }

    fn remove_level(&mut self, side: L3Side, price: Price) {
        let order_ids = self
            .side_mut(side)
            .remove(&price)
            .map(BTreeMap::into_values)
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        for order_id in order_ids {
            self.orders.remove(&order_id);
        }
    }

    fn append_side(&self, output: &mut Vec<L3Order>, side: L3Side, depth: usize) {
        match side {
            L3Side::Bid => {
                for queue in self.bids.values().rev().take(depth) {
                    output.extend(
                        queue
                            .values()
                            .filter_map(|order_id| self.orders.get(order_id).cloned()),
                    );
                }
            }
            L3Side::Ask => {
                for queue in self.asks.values().take(depth) {
                    output.extend(
                        queue
                            .values()
                            .filter_map(|order_id| self.orders.get(order_id).cloned()),
                    );
                }
            }
        }
    }

    fn side_mut(&mut self, side: L3Side) -> &mut BTreeMap<Price, BTreeMap<(u64, u64), L3OrderId>> {
        match side {
            L3Side::Bid => &mut self.bids,
            L3Side::Ask => &mut self.asks,
        }
    }
}

fn validate_levels(
    levels: &[BookLevel],
    allow_zero_quantity: bool,
    config: &BookConfig,
) -> Result<(), BookError> {
    if levels.len() > config.max_levels_per_side {
        return Err(BookError::Capacity);
    }
    for level in levels {
        if !allow_zero_quantity && level.quantity.value().is_zero() {
            return Err(BookError::InvalidBook);
        }
        if !level.quantity.value().is_zero() && level.order_count == Some(0) {
            return Err(BookError::InvalidBook);
        }
        if !decimal_is_multiple(level.price.value(), config.price_tick.value())
            || !decimal_is_multiple(level.quantity.value(), config.quantity_step.value())
        {
            return Err(BookError::MisalignedLevel);
        }
    }
    Ok(())
}

fn decimal_is_multiple(value: FixedDecimal, unit: FixedDecimal) -> bool {
    if unit.is_zero() {
        return false;
    }
    let scale = value.scale().max(unit.scale());
    let Some(value_factor) = 10_i128.checked_pow(scale - value.scale()) else {
        return false;
    };
    let Some(unit_factor) = 10_i128.checked_pow(scale - unit.scale()) else {
        return false;
    };
    let Some(value_mantissa) = value.mantissa().checked_mul(value_factor) else {
        return false;
    };
    let Some(unit_mantissa) = unit.mantissa().checked_mul(unit_factor) else {
        return false;
    };
    unit_mantissa != 0 && value_mantissa % unit_mantissa == 0
}

fn validate_l3_order(order: &L3Order, config: &BookConfig) -> Result<(), BookError> {
    if order.quantity.value().is_zero()
        || !decimal_is_multiple(order.price.value(), config.price_tick.value())
        || !decimal_is_multiple(order.quantity.value(), config.quantity_step.value())
    {
        Err(BookError::InvalidL3Order)
    } else {
        Ok(())
    }
}

fn strictly_descending(levels: &[BookLevel]) -> bool {
    levels.windows(2).all(|pair| pair[0].price > pair[1].price)
}

fn strictly_ascending(levels: &[BookLevel]) -> bool {
    levels.windows(2).all(|pair| pair[0].price < pair[1].price)
}

fn ensure_unique_prices(levels: &[BookLevel]) -> Result<(), BookError> {
    let mut seen = BTreeSet::new();
    if levels.iter().all(|level| seen.insert(level.price)) {
        Ok(())
    } else {
        Err(BookError::AmbiguousDelta)
    }
}

fn apply_side(side: &mut BTreeMap<Price, LevelValue>, levels: &[BookLevel]) {
    for level in levels {
        if level.quantity.value().is_zero() {
            side.remove(&level.price);
        } else {
            side.insert(level.price, level_value(level));
        }
    }
}

fn level_value(level: &BookLevel) -> LevelValue {
    LevelValue {
        quantity: level.quantity,
        order_count: level.order_count,
    }
}

fn truncate_bids(bids: &mut BTreeMap<Price, LevelValue>, depth: usize) {
    while bids.len() > depth {
        let Some((worst, _)) = bids.first_key_value() else {
            break;
        };
        let worst = *worst;
        bids.remove(&worst);
    }
}

fn truncate_asks(asks: &mut BTreeMap<Price, LevelValue>, depth: usize) {
    while asks.len() > depth {
        let Some((worst, _)) = asks.last_key_value() else {
            break;
        };
        let worst = *worst;
        asks.remove(&worst);
    }
}
