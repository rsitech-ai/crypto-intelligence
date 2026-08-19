use std::collections::BTreeSet;

use connector_core::DurableRawReference;
use domain::UnixNanos;
use event_envelope::{Side, VenueState};
use fixed_decimal::{FixedDecimal, Price, Quantity, Rate};
use serde::Deserialize;
use serde::de::{DeserializeOwned, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Value};
use thiserror::Error;

/// Native parser input is intentionally tighter than the raw-WAL ceiling.
pub const MAX_NATIVE_PAYLOAD_BYTES: usize = 1024 * 1024;
const MAX_DEPTH_LEVELS_PER_SIDE: usize = 5_000;
const MIN_PLAUSIBLE_MILLISECONDS: u64 = 1_000_000_000_000;
const MAX_PLAUSIBLE_MILLISECONDS: u64 = i64::MAX as u64 / 1_000_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BinanceInput {
    SpotWebSocket,
    UsdMAggregateTradeWebSocket,
    UsdMMarkPriceWebSocket,
    UsdMLiquidationWebSocket,
    UsdMDepthWebSocket,
    UsdMOpenInterestRest,
    SystemStatusRest,
}

impl BinanceInput {
    /// Canonical WAL stream contract for this transport route.
    ///
    /// Spot and USD-M public book snapshots share the corresponding market
    /// stream because snapshot and incremental records form one recovery
    /// protocol. Other provider transports remain separate.
    pub const fn wal_stream_name(self) -> &'static str {
        match self {
            Self::SpotWebSocket => "binance-spot-market-v1",
            Self::UsdMAggregateTradeWebSocket => "binance-usdm-aggregate-trade-v1",
            Self::UsdMMarkPriceWebSocket => "binance-usdm-mark-price-v1",
            Self::UsdMLiquidationWebSocket => "binance-usdm-liquidation-v1",
            Self::UsdMDepthWebSocket => "binance-usdm-depth-v1",
            Self::UsdMOpenInterestRest => "binance-usdm-open-interest-v1",
            Self::SystemStatusRest => "binance-system-status-v1",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BinanceMarket {
    Spot,
    UsdMarginedPerpetual,
}

impl BinanceMarket {
    pub const fn snapshot_wal_stream_name(self) -> &'static str {
        match self {
            Self::Spot => BinanceInput::SpotWebSocket.wal_stream_name(),
            Self::UsdMarginedPerpetual => BinanceInput::UsdMDepthWebSocket.wal_stream_name(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BinanceMessage {
    AggregateTrade(AggregateTrade),
    BookTicker(BookTicker),
    DepthUpdate(DepthUpdate),
    MarkPrice(MarkPrice),
    OpenInterest(OpenInterest),
    Liquidation(Liquidation),
    VenueStatus { state: VenueState, message: String },
}

/// A parsed message cryptographically bound to one acknowledged raw WAL record.
///
/// The private fields prevent callers from pairing a parsed payload with an
/// unrelated durable receipt before normalization.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DurableBinanceMessage {
    message: BinanceMessage,
    raw_payload_hash: [u8; 32],
}

impl DurableBinanceMessage {
    pub(crate) fn into_parts(self) -> (BinanceMessage, [u8; 32]) {
        (self.message, self.raw_payload_hash)
    }
}

/// A parsed depth snapshot cryptographically bound to one acknowledged raw WAL record.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DurableDepthSnapshot {
    snapshot: DepthSnapshot,
    raw_payload_hash: [u8; 32],
}

impl DurableDepthSnapshot {
    pub(crate) fn into_parts(self) -> (DepthSnapshot, [u8; 32]) {
        (self.snapshot, self.raw_payload_hash)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AggregateTrade {
    pub market: BinanceMarket,
    pub symbol: String,
    pub aggregate_trade_id: u64,
    pub price: Price,
    pub quantity: Quantity,
    pub normal_quantity: Option<Quantity>,
    pub first_trade_id: u64,
    pub last_trade_id: u64,
    pub event_time: UnixNanos,
    pub trade_time: UnixNanos,
    pub buyer_is_market_maker: bool,
    pub best_match: Option<bool>,
    pub market_category: Option<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BookTicker {
    pub symbol: String,
    pub update_id: u64,
    pub bid_price: Price,
    pub bid_quantity: Quantity,
    pub ask_price: Price,
    pub ask_quantity: Quantity,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeBookLevel {
    pub price: Price,
    pub quantity: Quantity,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DepthUpdate {
    pub market: BinanceMarket,
    pub symbol: String,
    pub pair_symbol: Option<String>,
    pub event_time: UnixNanos,
    pub transaction_time: Option<UnixNanos>,
    pub first_update_id: u64,
    pub final_update_id: u64,
    pub previous_final_update_id: Option<u64>,
    pub bids: Vec<NativeBookLevel>,
    pub asks: Vec<NativeBookLevel>,
    pub market_category: Option<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DepthSnapshot {
    pub market: BinanceMarket,
    pub symbol: String,
    pub last_update_id: u64,
    pub event_time: Option<UnixNanos>,
    pub transaction_time: Option<UnixNanos>,
    pub bids: Vec<NativeBookLevel>,
    pub asks: Vec<NativeBookLevel>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MarkPrice {
    pub symbol: String,
    pub event_time: UnixNanos,
    pub mark_price: Price,
    pub index_price: Price,
    pub estimated_settle_price: FixedDecimal,
    pub funding_rate: Rate,
    pub moving_average_price: Price,
    pub next_funding_time: UnixNanos,
    pub market_category: u8,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OpenInterest {
    pub symbol: String,
    pub quantity: Quantity,
    pub observed_at: UnixNanos,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Liquidation {
    pub symbol: String,
    pub pair_symbol: String,
    pub event_time: UnixNanos,
    pub transaction_time: UnixNanos,
    pub side: Side,
    pub order_type: String,
    pub time_in_force: String,
    pub status: String,
    pub original_quantity: Quantity,
    pub original_price: Price,
    pub average_price: Price,
    pub last_filled_quantity: Quantity,
    pub filled_quantity: Quantity,
    pub market_category: u8,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum NativeParseError {
    #[error("native payload is empty")]
    EmptyPayload,
    #[error("native payload exceeds the parser bound")]
    PayloadTooLarge,
    #[error("native payload is malformed")]
    MalformedJson,
    #[error("native payload contains undocumented fields")]
    SchemaDrift,
    #[error("native payload arrived on the wrong route")]
    WrongRoute,
    #[error("native event type is unsupported")]
    UnsupportedEvent,
    #[error("native market category is unsupported")]
    UnsupportedMarketCategory,
    #[error("native symbol is invalid")]
    InvalidSymbol,
    #[error("native decimal is invalid")]
    InvalidDecimal,
    #[error("native timestamp is not a plausible millisecond timestamp")]
    InvalidTimestamp,
    #[error("native sequence or identifier range is invalid")]
    InvalidSequence,
    #[error("native collection exceeds its bound")]
    TooManyLevels,
    #[error("native value is invalid")]
    InvalidValue,
    #[error("native payload does not match the acknowledged raw WAL record")]
    RawPayloadMismatch,
    #[error("native route does not match the acknowledged WAL stream contract")]
    RawStreamMismatch,
}

pub fn parse_native_message(
    input: BinanceInput,
    raw: &[u8],
) -> Result<BinanceMessage, NativeParseError> {
    let value = parse_unique_json_bounded(raw, MAX_NATIVE_PAYLOAD_BYTES)?;
    let event = value
        .as_object()
        .ok_or(NativeParseError::MalformedJson)
        .and_then(event_name)?
        .map(str::to_owned);
    match input {
        BinanceInput::SpotWebSocket => parse_spot(value, event.as_deref()),
        BinanceInput::UsdMAggregateTradeWebSocket => parse_usdm_trade(value, event.as_deref()),
        BinanceInput::UsdMMarkPriceWebSocket => parse_usdm_mark(value, event.as_deref()),
        BinanceInput::UsdMLiquidationWebSocket => parse_usdm_liquidation(value, event.as_deref()),
        BinanceInput::UsdMDepthWebSocket => parse_usdm_depth(value, event.as_deref()),
        BinanceInput::UsdMOpenInterestRest => parse_open_interest(value),
        BinanceInput::SystemStatusRest => parse_system_status(value),
    }
}

pub fn parse_durable_native_message(
    input: BinanceInput,
    raw: &[u8],
    reference: &DurableRawReference,
) -> Result<DurableBinanceMessage, NativeParseError> {
    let raw_payload_hash = *blake3::hash(raw).as_bytes();
    if &raw_payload_hash != reference.payload_hash() {
        return Err(NativeParseError::RawPayloadMismatch);
    }
    if reference.stream_name() != input.wal_stream_name() {
        return Err(NativeParseError::RawStreamMismatch);
    }
    Ok(DurableBinanceMessage {
        message: parse_native_message(input, raw)?,
        raw_payload_hash,
    })
}

pub fn parse_depth_snapshot(
    market: BinanceMarket,
    symbol: &str,
    raw: &[u8],
) -> Result<DepthSnapshot, NativeParseError> {
    validate_symbol(symbol)?;
    let value = parse_unique_json_bounded(raw, MAX_NATIVE_PAYLOAD_BYTES)?;
    let allowed = match market {
        BinanceMarket::Spot => &["lastUpdateId", "bids", "asks"][..],
        BinanceMarket::UsdMarginedPerpetual => &["lastUpdateId", "E", "T", "bids", "asks"][..],
    };
    let wire: WireDepthSnapshot = decode(value, allowed)?;
    if wire.last_update_id == 0
        || wire.bids.len() > MAX_DEPTH_LEVELS_PER_SIDE
        || wire.asks.len() > MAX_DEPTH_LEVELS_PER_SIDE
    {
        return Err(NativeParseError::InvalidSequence);
    }
    if wire.bids.is_empty() && wire.asks.is_empty() {
        return Err(NativeParseError::InvalidValue);
    }
    let (event_time, transaction_time) = match market {
        BinanceMarket::Spot if wire.event_time.is_none() && wire.transaction_time.is_none() => {
            (None, None)
        }
        BinanceMarket::UsdMarginedPerpetual
            if wire.event_time.is_some() && wire.transaction_time.is_some() =>
        {
            (
                wire.event_time.map(timestamp).transpose()?,
                wire.transaction_time.map(timestamp).transpose()?,
            )
        }
        _ => return Err(NativeParseError::MalformedJson),
    };
    Ok(DepthSnapshot {
        market,
        symbol: symbol.to_owned(),
        last_update_id: wire.last_update_id,
        event_time,
        transaction_time,
        bids: snapshot_levels(wire.bids)?,
        asks: snapshot_levels(wire.asks)?,
    })
}

pub fn parse_durable_depth_snapshot(
    market: BinanceMarket,
    symbol: &str,
    raw: &[u8],
    reference: &DurableRawReference,
) -> Result<DurableDepthSnapshot, NativeParseError> {
    let raw_payload_hash = *blake3::hash(raw).as_bytes();
    if &raw_payload_hash != reference.payload_hash() {
        return Err(NativeParseError::RawPayloadMismatch);
    }
    if reference.stream_name() != market.snapshot_wal_stream_name() {
        return Err(NativeParseError::RawStreamMismatch);
    }
    Ok(DurableDepthSnapshot {
        snapshot: parse_depth_snapshot(market, symbol, raw)?,
        raw_payload_hash,
    })
}

pub(crate) fn parse_unique_json_bounded(
    raw: &[u8],
    maximum_bytes: usize,
) -> Result<Value, NativeParseError> {
    if raw.len() > maximum_bytes {
        return Err(NativeParseError::PayloadTooLarge);
    }
    if raw.is_empty() || raw.iter().all(u8::is_ascii_whitespace) {
        return Err(NativeParseError::EmptyPayload);
    }
    let mut deserializer = serde_json::Deserializer::from_slice(raw);
    let UniqueValue(value) =
        UniqueValue::deserialize(&mut deserializer).map_err(|_| NativeParseError::MalformedJson)?;
    deserializer
        .end()
        .map_err(|_| NativeParseError::MalformedJson)?;
    Ok(value)
}

fn parse_spot(value: Value, event: Option<&str>) -> Result<BinanceMessage, NativeParseError> {
    match event {
        Some("aggTrade") => parse_trade(value, BinanceMarket::Spot),
        Some("depthUpdate") => parse_depth(value, BinanceMarket::Spot),
        Some(_) => Err(NativeParseError::WrongRoute),
        None => parse_book_ticker(value),
    }
}

fn parse_usdm_trade(value: Value, event: Option<&str>) -> Result<BinanceMessage, NativeParseError> {
    match event {
        Some("aggTrade") => parse_trade(value, BinanceMarket::UsdMarginedPerpetual),
        Some(_) => Err(NativeParseError::WrongRoute),
        None => Err(NativeParseError::MalformedJson),
    }
}

fn parse_usdm_mark(value: Value, event: Option<&str>) -> Result<BinanceMessage, NativeParseError> {
    match event {
        Some("markPriceUpdate") => parse_mark_price(value),
        Some(_) => Err(NativeParseError::WrongRoute),
        None => Err(NativeParseError::MalformedJson),
    }
}

fn parse_usdm_liquidation(
    value: Value,
    event: Option<&str>,
) -> Result<BinanceMessage, NativeParseError> {
    match event {
        Some("forceOrder") => parse_liquidation(value),
        Some(_) => Err(NativeParseError::WrongRoute),
        None => Err(NativeParseError::MalformedJson),
    }
}

fn parse_usdm_depth(value: Value, event: Option<&str>) -> Result<BinanceMessage, NativeParseError> {
    match event {
        Some("depthUpdate") => parse_depth(value, BinanceMarket::UsdMarginedPerpetual),
        Some(_) => Err(NativeParseError::WrongRoute),
        None => Err(NativeParseError::MalformedJson),
    }
}

fn event_name(object: &Map<String, Value>) -> Result<Option<&str>, NativeParseError> {
    object
        .get("e")
        .map(|value| value.as_str().ok_or(NativeParseError::MalformedJson))
        .transpose()
}

fn parse_trade(value: Value, market: BinanceMarket) -> Result<BinanceMessage, NativeParseError> {
    let allowed = match market {
        BinanceMarket::Spot => &["e", "E", "s", "a", "p", "q", "f", "l", "T", "m", "M"][..],
        BinanceMarket::UsdMarginedPerpetual => {
            &["e", "E", "s", "a", "p", "q", "nq", "f", "l", "T", "m", "st"][..]
        }
    };
    let wire: WireAggregateTrade = decode(value, allowed)?;
    validate_symbol(&wire.symbol)?;
    validate_id_range(
        wire.aggregate_trade_id,
        wire.first_trade_id,
        wire.last_trade_id,
    )?;
    validate_market_category(market, wire.market_category)?;
    match market {
        BinanceMarket::Spot
            if wire.best_match.is_none()
                || wire.normal_quantity.is_some()
                || wire.market_category.is_some() =>
        {
            return Err(NativeParseError::MalformedJson);
        }
        BinanceMarket::UsdMarginedPerpetual
            if wire.best_match.is_some()
                || wire.normal_quantity.is_none()
                || wire.market_category.is_none() =>
        {
            return Err(NativeParseError::MalformedJson);
        }
        _ => {}
    }
    let trade_quantity = quantity(&wire.quantity)?;
    if trade_quantity.value().is_zero() {
        return Err(NativeParseError::InvalidValue);
    }
    let normal_quantity = wire.normal_quantity.as_deref().map(quantity).transpose()?;
    if normal_quantity.is_some_and(|normal| normal > trade_quantity) {
        return Err(NativeParseError::InvalidValue);
    }
    Ok(BinanceMessage::AggregateTrade(AggregateTrade {
        market,
        symbol: wire.symbol,
        aggregate_trade_id: wire.aggregate_trade_id,
        price: price(&wire.price)?,
        quantity: trade_quantity,
        normal_quantity,
        first_trade_id: wire.first_trade_id,
        last_trade_id: wire.last_trade_id,
        event_time: timestamp(wire.event_time)?,
        trade_time: timestamp(wire.trade_time)?,
        buyer_is_market_maker: wire.buyer_is_market_maker,
        best_match: wire.best_match,
        market_category: wire.market_category,
    }))
}

fn parse_book_ticker(value: Value) -> Result<BinanceMessage, NativeParseError> {
    let wire: WireBookTicker = decode(value, &["u", "s", "b", "B", "a", "A"])?;
    validate_symbol(&wire.symbol)?;
    if wire.update_id == 0 {
        return Err(NativeParseError::InvalidSequence);
    }
    Ok(BinanceMessage::BookTicker(BookTicker {
        symbol: wire.symbol,
        update_id: wire.update_id,
        bid_price: price(&wire.bid_price)?,
        bid_quantity: positive_quantity(&wire.bid_quantity)?,
        ask_price: price(&wire.ask_price)?,
        ask_quantity: positive_quantity(&wire.ask_quantity)?,
    }))
}

fn parse_depth(value: Value, market: BinanceMarket) -> Result<BinanceMessage, NativeParseError> {
    let allowed = match market {
        BinanceMarket::Spot => &["e", "E", "s", "U", "u", "b", "a"][..],
        BinanceMarket::UsdMarginedPerpetual => {
            &["e", "E", "T", "s", "ps", "U", "u", "pu", "b", "a", "st"][..]
        }
    };
    let wire: WireDepth = decode(value, allowed)?;
    validate_symbol(&wire.symbol)?;
    if let Some(pair) = wire.pair_symbol.as_deref() {
        validate_symbol(pair)?;
        if pair != wire.symbol {
            return Err(NativeParseError::InvalidSymbol);
        }
    }
    if wire.first_update_id == 0
        || wire.final_update_id == 0
        || wire.first_update_id > wire.final_update_id
        || market == BinanceMarket::Spot && wire.previous_final_update_id.is_some()
        || market == BinanceMarket::UsdMarginedPerpetual
            && wire
                .previous_final_update_id
                .is_none_or(|previous| previous >= wire.first_update_id)
    {
        return Err(NativeParseError::InvalidSequence);
    }
    if market == BinanceMarket::UsdMarginedPerpetual
        && (wire.pair_symbol.is_none() || wire.transaction_time.is_none())
    {
        return Err(NativeParseError::MalformedJson);
    }
    validate_market_category(market, wire.market_category)?;
    if wire.bids.len() > MAX_DEPTH_LEVELS_PER_SIDE || wire.asks.len() > MAX_DEPTH_LEVELS_PER_SIDE {
        return Err(NativeParseError::TooManyLevels);
    }
    if wire.bids.is_empty() && wire.asks.is_empty() {
        return Err(NativeParseError::InvalidValue);
    }
    Ok(BinanceMessage::DepthUpdate(DepthUpdate {
        market,
        symbol: wire.symbol,
        pair_symbol: wire.pair_symbol,
        event_time: timestamp(wire.event_time)?,
        transaction_time: wire.transaction_time.map(timestamp).transpose()?,
        first_update_id: wire.first_update_id,
        final_update_id: wire.final_update_id,
        previous_final_update_id: wire.previous_final_update_id,
        bids: levels(wire.bids)?,
        asks: levels(wire.asks)?,
        market_category: wire.market_category,
    }))
}

fn parse_mark_price(value: Value) -> Result<BinanceMessage, NativeParseError> {
    let wire: WireMarkPrice = decode(value, &["e", "E", "s", "p", "i", "P", "r", "ap", "T", "st"])?;
    validate_symbol(&wire.symbol)?;
    require_usdm(wire.market_category)?;
    let estimated = decimal(&wire.estimated_settle_price)?;
    if estimated.is_negative() {
        return Err(NativeParseError::InvalidDecimal);
    }
    let event_time = timestamp(wire.event_time)?;
    let next_funding_time = timestamp(wire.next_funding_time)?;
    if next_funding_time <= event_time {
        return Err(NativeParseError::InvalidTimestamp);
    }
    Ok(BinanceMessage::MarkPrice(MarkPrice {
        symbol: wire.symbol,
        event_time,
        mark_price: price(&wire.mark_price)?,
        index_price: price(&wire.index_price)?,
        estimated_settle_price: estimated,
        funding_rate: rate(&wire.funding_rate)?,
        moving_average_price: price(&wire.moving_average_price)?,
        next_funding_time,
        market_category: wire.market_category,
    }))
}

fn parse_open_interest(value: Value) -> Result<BinanceMessage, NativeParseError> {
    let wire: WireOpenInterest = decode(value, &["openInterest", "symbol", "time"])?;
    validate_symbol(&wire.symbol)?;
    Ok(BinanceMessage::OpenInterest(OpenInterest {
        symbol: wire.symbol,
        quantity: quantity(&wire.open_interest)?,
        observed_at: timestamp(wire.time)?,
    }))
}

fn parse_system_status(value: Value) -> Result<BinanceMessage, NativeParseError> {
    let wire: WireSystemStatus = decode(value, &["status", "msg"])?;
    if wire.message.is_empty()
        || wire.message.len() > 4_096
        || wire.message.trim() != wire.message
        || wire.message.chars().any(char::is_control)
    {
        return Err(NativeParseError::InvalidValue);
    }
    let state = match wire.status {
        0 => VenueState::Operational,
        1 => VenueState::Maintenance,
        _ => return Err(NativeParseError::InvalidValue),
    };
    Ok(BinanceMessage::VenueStatus {
        state,
        message: wire.message,
    })
}

fn parse_liquidation(value: Value) -> Result<BinanceMessage, NativeParseError> {
    let object = value.as_object().ok_or(NativeParseError::MalformedJson)?;
    ensure_allowed(object, &["e", "E", "ps", "st", "o"])?;
    let order = object
        .get("o")
        .and_then(Value::as_object)
        .ok_or(NativeParseError::MalformedJson)?;
    ensure_allowed(
        order,
        &["s", "S", "o", "f", "q", "p", "ap", "X", "l", "z", "T"],
    )?;
    let wire: WireLiquidation =
        serde_json::from_value(value).map_err(|_| NativeParseError::MalformedJson)?;
    require_usdm(wire.market_category)?;
    validate_symbol(&wire.order.symbol)?;
    validate_symbol(&wire.pair_symbol)?;
    if wire.pair_symbol != wire.order.symbol {
        return Err(NativeParseError::InvalidSymbol);
    }
    validate_token(&wire.order.order_type)?;
    validate_token(&wire.order.time_in_force)?;
    validate_token(&wire.order.status)?;
    let side = match wire.order.side.as_str() {
        "BUY" => Side::Buy,
        "SELL" => Side::Sell,
        _ => return Err(NativeParseError::InvalidValue),
    };
    Ok(BinanceMessage::Liquidation(Liquidation {
        symbol: wire.order.symbol,
        pair_symbol: wire.pair_symbol,
        event_time: timestamp(wire.event_time)?,
        transaction_time: timestamp(wire.order.trade_time)?,
        side,
        order_type: wire.order.order_type,
        time_in_force: wire.order.time_in_force,
        status: wire.order.status,
        original_quantity: positive_quantity(&wire.order.original_quantity)?,
        original_price: price(&wire.order.original_price)?,
        average_price: price(&wire.order.average_price)?,
        last_filled_quantity: positive_quantity(&wire.order.last_filled_quantity)?,
        filled_quantity: positive_quantity(&wire.order.filled_quantity)?,
        market_category: wire.market_category,
    }))
}

fn decode<T: DeserializeOwned>(value: Value, allowed: &[&str]) -> Result<T, NativeParseError> {
    let object = value.as_object().ok_or(NativeParseError::MalformedJson)?;
    ensure_allowed(object, allowed)?;
    serde_json::from_value(value).map_err(|_| NativeParseError::MalformedJson)
}

fn ensure_allowed(object: &Map<String, Value>, allowed: &[&str]) -> Result<(), NativeParseError> {
    let allowed = allowed.iter().copied().collect::<BTreeSet<_>>();
    if object.keys().any(|key| !allowed.contains(key.as_str())) {
        Err(NativeParseError::SchemaDrift)
    } else {
        Ok(())
    }
}

fn validate_symbol(symbol: &str) -> Result<(), NativeParseError> {
    if symbol.is_empty()
        || symbol.len() > 96
        || !symbol
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
    {
        Err(NativeParseError::InvalidSymbol)
    } else {
        Ok(())
    }
}

fn validate_token(value: &str) -> Result<(), NativeParseError> {
    if value.is_empty()
        || value.len() > 32
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
    {
        Err(NativeParseError::InvalidValue)
    } else {
        Ok(())
    }
}

fn validate_id_range(
    aggregate_trade_id: u64,
    first_trade_id: u64,
    last_trade_id: u64,
) -> Result<(), NativeParseError> {
    if aggregate_trade_id == 0
        || first_trade_id == 0
        || last_trade_id == 0
        || first_trade_id > last_trade_id
    {
        Err(NativeParseError::InvalidSequence)
    } else {
        Ok(())
    }
}

fn validate_market_category(
    market: BinanceMarket,
    market_category: Option<u8>,
) -> Result<(), NativeParseError> {
    match (market, market_category) {
        (BinanceMarket::Spot, None) => Ok(()),
        (BinanceMarket::UsdMarginedPerpetual, Some(category)) => require_usdm(category),
        _ => Err(NativeParseError::UnsupportedMarketCategory),
    }
}

fn require_usdm(category: u8) -> Result<(), NativeParseError> {
    if category == 1 {
        Ok(())
    } else {
        Err(NativeParseError::UnsupportedMarketCategory)
    }
}

fn timestamp(milliseconds: u64) -> Result<UnixNanos, NativeParseError> {
    if !(MIN_PLAUSIBLE_MILLISECONDS..=MAX_PLAUSIBLE_MILLISECONDS).contains(&milliseconds) {
        return Err(NativeParseError::InvalidTimestamp);
    }
    let nanoseconds = i64::try_from(milliseconds)
        .ok()
        .and_then(|value| value.checked_mul(1_000_000))
        .ok_or(NativeParseError::InvalidTimestamp)?;
    Ok(UnixNanos::new(nanoseconds))
}

fn decimal(value: &str) -> Result<FixedDecimal, NativeParseError> {
    FixedDecimal::parse(value).map_err(|_| NativeParseError::InvalidDecimal)
}

fn price(value: &str) -> Result<Price, NativeParseError> {
    Price::new(decimal(value)?).map_err(|_| NativeParseError::InvalidDecimal)
}

fn quantity(value: &str) -> Result<Quantity, NativeParseError> {
    Quantity::new(decimal(value)?).map_err(|_| NativeParseError::InvalidDecimal)
}

fn positive_quantity(value: &str) -> Result<Quantity, NativeParseError> {
    let value = quantity(value)?;
    if value.value().is_zero() {
        Err(NativeParseError::InvalidValue)
    } else {
        Ok(value)
    }
}

fn rate(value: &str) -> Result<Rate, NativeParseError> {
    Rate::new(decimal(value)?).map_err(|_| NativeParseError::InvalidDecimal)
}

fn levels(wire: Vec<[String; 2]>) -> Result<Vec<NativeBookLevel>, NativeParseError> {
    wire.into_iter()
        .map(|[price_value, quantity_value]| {
            Ok(NativeBookLevel {
                price: price(&price_value)?,
                quantity: quantity(&quantity_value)?,
            })
        })
        .collect()
}

fn snapshot_levels(wire: Vec<[String; 2]>) -> Result<Vec<NativeBookLevel>, NativeParseError> {
    wire.into_iter()
        .map(|[price_value, quantity_value]| {
            Ok(NativeBookLevel {
                price: price(&price_value)?,
                quantity: positive_quantity(&quantity_value)?,
            })
        })
        .collect()
}

struct UniqueValue(Value);

impl<'de> Deserialize<'de> for UniqueValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_any(UniqueValueVisitor)
    }
}

struct UniqueValueVisitor;

impl<'de> Visitor<'de> for UniqueValueVisitor {
    type Value = UniqueValue;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a JSON value without duplicate object keys")
    }

    fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
        Ok(UniqueValue(Value::Bool(value)))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
        Ok(UniqueValue(Value::Number(value.into())))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
        Ok(UniqueValue(Value::Number(value.into())))
    }

    fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        serde_json::Number::from_f64(value)
            .map(Value::Number)
            .map(UniqueValue)
            .ok_or_else(|| E::custom("non-finite JSON number"))
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
        Ok(UniqueValue(Value::String(value.to_owned())))
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
        Ok(UniqueValue(Value::String(value)))
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(UniqueValue(Value::Null))
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(UniqueValue(Value::Null))
    }

    fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        UniqueValue::deserialize(deserializer)
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut values = Vec::with_capacity(sequence.size_hint().unwrap_or(0).min(1024));
        while let Some(UniqueValue(value)) = sequence.next_element()? {
            values.push(value);
        }
        Ok(UniqueValue(Value::Array(values)))
    }

    fn visit_map<A>(self, mut entries: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut values = Map::new();
        while let Some((key, UniqueValue(value))) = entries.next_entry::<String, UniqueValue>()? {
            if values.insert(key, value).is_some() {
                return Err(serde::de::Error::custom("duplicate JSON object key"));
            }
        }
        Ok(UniqueValue(Value::Object(values)))
    }
}

#[derive(Deserialize)]
struct WireAggregateTrade {
    #[serde(rename = "E")]
    event_time: u64,
    #[serde(rename = "s")]
    symbol: String,
    #[serde(rename = "a")]
    aggregate_trade_id: u64,
    #[serde(rename = "p")]
    price: String,
    #[serde(rename = "q")]
    quantity: String,
    #[serde(rename = "nq")]
    normal_quantity: Option<String>,
    #[serde(rename = "f")]
    first_trade_id: u64,
    #[serde(rename = "l")]
    last_trade_id: u64,
    #[serde(rename = "T")]
    trade_time: u64,
    #[serde(rename = "m")]
    buyer_is_market_maker: bool,
    #[serde(rename = "M")]
    best_match: Option<bool>,
    #[serde(rename = "st")]
    market_category: Option<u8>,
}

#[derive(Deserialize)]
struct WireBookTicker {
    #[serde(rename = "u")]
    update_id: u64,
    #[serde(rename = "s")]
    symbol: String,
    #[serde(rename = "b")]
    bid_price: String,
    #[serde(rename = "B")]
    bid_quantity: String,
    #[serde(rename = "a")]
    ask_price: String,
    #[serde(rename = "A")]
    ask_quantity: String,
}

#[derive(Deserialize)]
struct WireDepth {
    #[serde(rename = "E")]
    event_time: u64,
    #[serde(rename = "T")]
    transaction_time: Option<u64>,
    #[serde(rename = "s")]
    symbol: String,
    #[serde(rename = "ps")]
    pair_symbol: Option<String>,
    #[serde(rename = "U")]
    first_update_id: u64,
    #[serde(rename = "u")]
    final_update_id: u64,
    #[serde(rename = "pu")]
    previous_final_update_id: Option<u64>,
    #[serde(rename = "b")]
    bids: Vec<[String; 2]>,
    #[serde(rename = "a")]
    asks: Vec<[String; 2]>,
    #[serde(rename = "st")]
    market_category: Option<u8>,
}

#[derive(Deserialize)]
struct WireDepthSnapshot {
    #[serde(rename = "lastUpdateId")]
    last_update_id: u64,
    #[serde(rename = "E")]
    event_time: Option<u64>,
    #[serde(rename = "T")]
    transaction_time: Option<u64>,
    bids: Vec<[String; 2]>,
    asks: Vec<[String; 2]>,
}

#[derive(Deserialize)]
struct WireMarkPrice {
    #[serde(rename = "E")]
    event_time: u64,
    #[serde(rename = "s")]
    symbol: String,
    #[serde(rename = "p")]
    mark_price: String,
    #[serde(rename = "i")]
    index_price: String,
    #[serde(rename = "P")]
    estimated_settle_price: String,
    #[serde(rename = "r")]
    funding_rate: String,
    #[serde(rename = "ap")]
    moving_average_price: String,
    #[serde(rename = "T")]
    next_funding_time: u64,
    #[serde(rename = "st")]
    market_category: u8,
}

#[derive(Deserialize)]
struct WireOpenInterest {
    #[serde(rename = "openInterest")]
    open_interest: String,
    symbol: String,
    time: u64,
}

#[derive(Deserialize)]
struct WireSystemStatus {
    status: u8,
    #[serde(rename = "msg")]
    message: String,
}

#[derive(Deserialize)]
struct WireLiquidation {
    #[serde(rename = "E")]
    event_time: u64,
    #[serde(rename = "ps")]
    pair_symbol: String,
    #[serde(rename = "st")]
    market_category: u8,
    #[serde(rename = "o")]
    order: WireLiquidationOrder,
}

#[derive(Deserialize)]
struct WireLiquidationOrder {
    #[serde(rename = "s")]
    symbol: String,
    #[serde(rename = "S")]
    side: String,
    #[serde(rename = "o")]
    order_type: String,
    #[serde(rename = "f")]
    time_in_force: String,
    #[serde(rename = "q")]
    original_quantity: String,
    #[serde(rename = "p")]
    original_price: String,
    #[serde(rename = "ap")]
    average_price: String,
    #[serde(rename = "X")]
    status: String,
    #[serde(rename = "l")]
    last_filled_quantity: String,
    #[serde(rename = "z")]
    filled_quantity: String,
    #[serde(rename = "T")]
    trade_time: u64,
}
