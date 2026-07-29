use std::collections::BTreeSet;

use connector_core::DurableRawReference;
use domain::UnixNanos;
use event_envelope::Side;
use fixed_decimal::{FixedDecimal, Price, Quantity, Rate};
use serde::Deserialize;
use serde::de::{MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Value};
use thiserror::Error;

/// Native parser input is intentionally tighter than the raw-WAL ceiling.
pub const MAX_NATIVE_PAYLOAD_BYTES: usize = 1024 * 1024;
const MAX_BOOK_LEVELS_PER_SIDE: usize = 1_000;
const MAX_TRADES_PER_MESSAGE: usize = 1_024;
const MAX_LIQUIDATIONS_PER_MESSAGE: usize = 4_096;
const MIN_PLAUSIBLE_MILLISECONDS: u64 = 1_000_000_000_000;
const MAX_PLAUSIBLE_MILLISECONDS: u64 = i64::MAX as u64 / 1_000_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BybitMarket {
    Spot,
    LinearPerpetual,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BybitInput {
    PublicWebSocket(BybitMarket),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BookMessageKind {
    Snapshot,
    Delta,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BybitMessage {
    OrderBook(OrderBookMessage),
    PublicTrades(Vec<PublicTrade>),
    LinearTicker(LinearTicker),
    AllLiquidations(Vec<AllLiquidation>),
}

/// A parsed Bybit payload cryptographically bound to one acknowledged raw WAL record.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DurableBybitMessage {
    message: BybitMessage,
    raw_payload_hash: [u8; 32],
}

impl DurableBybitMessage {
    pub(crate) fn into_parts(self) -> (BybitMessage, [u8; 32]) {
        (self.message, self.raw_payload_hash)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeBookLevel {
    pub price: Price,
    pub quantity: Quantity,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OrderBookMessage {
    pub market: BybitMarket,
    pub symbol: String,
    pub depth: u32,
    pub kind: BookMessageKind,
    pub update_id: u64,
    pub cross_sequence: u64,
    pub event_time: UnixNanos,
    pub matching_engine_time: UnixNanos,
    pub bids: Vec<NativeBookLevel>,
    pub asks: Vec<NativeBookLevel>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PublicTrade {
    pub market: BybitMarket,
    pub symbol: String,
    pub trade_id: String,
    pub side: Side,
    pub price: Price,
    pub quantity: Quantity,
    pub event_time: UnixNanos,
    pub trade_time: UnixNanos,
    pub tick_direction: Option<String>,
    pub block_trade: bool,
    pub rpi: bool,
    pub cross_sequence: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LinearTicker {
    pub symbol: String,
    pub event_time: UnixNanos,
    pub cross_sequence: u64,
    pub mark_price: Price,
    pub index_price: Price,
    pub open_interest: Quantity,
    pub open_interest_value: FixedDecimal,
    pub funding_rate: Rate,
    pub next_funding_time: UnixNanos,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AllLiquidation {
    pub symbol: String,
    pub side: Side,
    pub quantity: Quantity,
    pub bankruptcy_price: Price,
    pub event_time: UnixNanos,
    pub transaction_time: UnixNanos,
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
    #[error("native topic is unsupported")]
    UnsupportedTopic,
    #[error("native symbol is invalid")]
    InvalidSymbol,
    #[error("native decimal is invalid")]
    InvalidDecimal,
    #[error("native timestamp is not a plausible millisecond timestamp")]
    InvalidTimestamp,
    #[error("native sequence or identifier range is invalid")]
    InvalidSequence,
    #[error("native collection exceeds its bound")]
    CollectionTooLarge,
    #[error("native value is invalid")]
    InvalidValue,
    #[error("native payload does not match the acknowledged raw WAL record")]
    RawPayloadMismatch,
}

pub fn parse_native_message(
    input: BybitInput,
    raw: &[u8],
) -> Result<BybitMessage, NativeParseError> {
    let value = parse_unique_json_bounded(raw, MAX_NATIVE_PAYLOAD_BYTES)?;
    let object = value.as_object().ok_or(NativeParseError::MalformedJson)?;
    let topic = object
        .get("topic")
        .and_then(Value::as_str)
        .ok_or(NativeParseError::MalformedJson)?;
    let BybitInput::PublicWebSocket(market) = input;
    if topic.starts_with("orderbook.") {
        parse_orderbook(value, market)
    } else if topic.starts_with("publicTrade.") {
        parse_trades(value, market)
    } else if topic.starts_with("tickers.") {
        if market != BybitMarket::LinearPerpetual {
            return Err(NativeParseError::WrongRoute);
        }
        parse_linear_ticker(value)
    } else if topic.starts_with("allLiquidation.") {
        if market != BybitMarket::LinearPerpetual {
            return Err(NativeParseError::WrongRoute);
        }
        parse_all_liquidations(value)
    } else {
        Err(NativeParseError::UnsupportedTopic)
    }
}

pub fn parse_durable_native_message(
    input: BybitInput,
    raw: &[u8],
    reference: &DurableRawReference,
) -> Result<DurableBybitMessage, NativeParseError> {
    let raw_payload_hash = *blake3::hash(raw).as_bytes();
    if &raw_payload_hash != reference.payload_hash() {
        return Err(NativeParseError::RawPayloadMismatch);
    }
    Ok(DurableBybitMessage {
        message: parse_native_message(input, raw)?,
        raw_payload_hash,
    })
}

fn parse_orderbook(value: Value, market: BybitMarket) -> Result<BybitMessage, NativeParseError> {
    let object = value.as_object().ok_or(NativeParseError::MalformedJson)?;
    ensure_allowed(object, &["topic", "type", "ts", "data", "cts"])?;
    let data = object
        .get("data")
        .and_then(Value::as_object)
        .ok_or(NativeParseError::MalformedJson)?;
    ensure_allowed(data, &["s", "b", "a", "u", "seq"])?;
    let wire: WireOrderBook =
        serde_json::from_value(value).map_err(|_| NativeParseError::MalformedJson)?;
    let (depth, topic_symbol) = parse_orderbook_topic(&wire.topic)?;
    validate_symbol(&wire.data.symbol)?;
    if topic_symbol != wire.data.symbol {
        return Err(NativeParseError::InvalidSymbol);
    }
    let kind = match wire.message_type.as_str() {
        "snapshot" => BookMessageKind::Snapshot,
        "delta" => BookMessageKind::Delta,
        _ => return Err(NativeParseError::InvalidValue),
    };
    if wire.data.update_id == 0
        || wire.data.cross_sequence == 0
        || wire.data.update_id == 1 && kind != BookMessageKind::Snapshot
    {
        return Err(NativeParseError::InvalidSequence);
    }
    if wire.data.bids.len() > MAX_BOOK_LEVELS_PER_SIDE
        || wire.data.asks.len() > MAX_BOOK_LEVELS_PER_SIDE
    {
        return Err(NativeParseError::CollectionTooLarge);
    }
    if wire.data.bids.is_empty() && wire.data.asks.is_empty() {
        return Err(NativeParseError::InvalidValue);
    }
    let bids = levels(wire.data.bids, kind)?;
    let asks = levels(wire.data.asks, kind)?;
    Ok(BybitMessage::OrderBook(OrderBookMessage {
        market,
        symbol: wire.data.symbol,
        depth,
        kind,
        update_id: wire.data.update_id,
        cross_sequence: wire.data.cross_sequence,
        event_time: timestamp(wire.event_time)?,
        matching_engine_time: timestamp(wire.matching_engine_time)?,
        bids,
        asks,
    }))
}

fn parse_trades(value: Value, market: BybitMarket) -> Result<BybitMessage, NativeParseError> {
    let object = value.as_object().ok_or(NativeParseError::MalformedJson)?;
    ensure_allowed(object, &["topic", "type", "ts", "data"])?;
    let data = object
        .get("data")
        .and_then(Value::as_array)
        .ok_or(NativeParseError::MalformedJson)?;
    for trade in data {
        let trade = trade.as_object().ok_or(NativeParseError::MalformedJson)?;
        ensure_allowed(
            trade,
            &["T", "s", "S", "v", "p", "L", "i", "BT", "RPI", "seq"],
        )?;
    }
    let wire: WireTrades =
        serde_json::from_value(value).map_err(|_| NativeParseError::MalformedJson)?;
    if wire.message_type != "snapshot" {
        return Err(NativeParseError::InvalidValue);
    }
    let topic_symbol = parse_simple_topic(&wire.topic, "publicTrade.")?;
    timestamp(wire.event_time)?;
    if wire.data.is_empty() || wire.data.len() > MAX_TRADES_PER_MESSAGE {
        return Err(if wire.data.is_empty() {
            NativeParseError::InvalidValue
        } else {
            NativeParseError::CollectionTooLarge
        });
    }
    let event_time = timestamp(wire.event_time)?;
    let mut seen_trade_ids = BTreeSet::new();
    let trades = wire
        .data
        .into_iter()
        .map(|trade| {
            validate_symbol(&trade.symbol)?;
            if trade.symbol != topic_symbol {
                return Err(NativeParseError::InvalidSymbol);
            }
            validate_trade_id(&trade.trade_id)?;
            if !seen_trade_ids.insert(trade.trade_id.clone()) || trade.cross_sequence == 0 {
                return Err(NativeParseError::InvalidSequence);
            }
            match market {
                BybitMarket::Spot if trade.tick_direction.is_some() => {
                    return Err(NativeParseError::WrongRoute);
                }
                BybitMarket::LinearPerpetual if trade.tick_direction.is_none() => {
                    return Err(NativeParseError::WrongRoute);
                }
                _ => {}
            }
            let side = parse_side(&trade.side)?;
            Ok(PublicTrade {
                market,
                symbol: trade.symbol,
                trade_id: trade.trade_id,
                side,
                price: price(&trade.price)?,
                quantity: positive_quantity(&trade.quantity)?,
                event_time,
                trade_time: timestamp(trade.trade_time)?,
                tick_direction: trade.tick_direction,
                block_trade: trade.block_trade,
                rpi: trade.rpi.unwrap_or(false),
                cross_sequence: trade.cross_sequence,
            })
        })
        .collect::<Result<Vec<_>, NativeParseError>>()?;
    Ok(BybitMessage::PublicTrades(trades))
}

fn parse_linear_ticker(value: Value) -> Result<BybitMessage, NativeParseError> {
    let object = value.as_object().ok_or(NativeParseError::MalformedJson)?;
    ensure_allowed(object, &["topic", "type", "data", "cs", "ts"])?;
    let data = object
        .get("data")
        .and_then(Value::as_object)
        .ok_or(NativeParseError::MalformedJson)?;
    ensure_allowed(
        data,
        &[
            "symbol",
            "tickDirection",
            "price24hPcnt",
            "lastPrice",
            "prevPrice24h",
            "highPrice24h",
            "lowPrice24h",
            "prevPrice1h",
            "markPrice",
            "indexPrice",
            "openInterest",
            "openInterestValue",
            "singleOpenInterest",
            "singleOpenInterestValue",
            "turnover24h",
            "volume24h",
            "fundingIntervalHour",
            "fundingCap",
            "nextFundingTime",
            "fundingRate",
            "bid1Price",
            "bid1Size",
            "ask1Price",
            "ask1Size",
            "preOpenPrice",
            "preQty",
            "curPreListingPhase",
        ],
    )?;
    let wire: WireLinearTicker =
        serde_json::from_value(value).map_err(|_| NativeParseError::MalformedJson)?;
    if !matches!(wire.message_type.as_str(), "snapshot" | "delta") || wire.cross_sequence == 0 {
        return Err(NativeParseError::InvalidSequence);
    }
    let topic_symbol = parse_simple_topic(&wire.topic, "tickers.")?;
    validate_symbol(&wire.data.symbol)?;
    if wire.data.symbol != topic_symbol {
        return Err(NativeParseError::InvalidSymbol);
    }
    let next_funding_time = wire
        .data
        .next_funding_time
        .parse::<u64>()
        .map_err(|_| NativeParseError::InvalidTimestamp)
        .and_then(timestamp)?;
    let event_time = timestamp(wire.event_time)?;
    if next_funding_time <= event_time {
        return Err(NativeParseError::InvalidTimestamp);
    }
    let open_interest_value = decimal(&wire.data.open_interest_value)?;
    if open_interest_value.is_negative() {
        return Err(NativeParseError::InvalidDecimal);
    }
    Ok(BybitMessage::LinearTicker(LinearTicker {
        symbol: wire.data.symbol,
        event_time,
        cross_sequence: wire.cross_sequence,
        mark_price: price(&wire.data.mark_price)?,
        index_price: price(&wire.data.index_price)?,
        open_interest: quantity(&wire.data.open_interest)?,
        open_interest_value,
        funding_rate: rate(&wire.data.funding_rate)?,
        next_funding_time,
    }))
}

fn parse_all_liquidations(value: Value) -> Result<BybitMessage, NativeParseError> {
    let object = value.as_object().ok_or(NativeParseError::MalformedJson)?;
    ensure_allowed(object, &["topic", "type", "ts", "data"])?;
    let data = object
        .get("data")
        .and_then(Value::as_array)
        .ok_or(NativeParseError::MalformedJson)?;
    for liquidation in data {
        let liquidation = liquidation
            .as_object()
            .ok_or(NativeParseError::MalformedJson)?;
        ensure_allowed(liquidation, &["T", "s", "S", "v", "p"])?;
    }
    let wire: WireAllLiquidations =
        serde_json::from_value(value).map_err(|_| NativeParseError::MalformedJson)?;
    if wire.message_type != "snapshot"
        || wire.data.is_empty()
        || wire.data.len() > MAX_LIQUIDATIONS_PER_MESSAGE
    {
        return Err(if wire.data.len() > MAX_LIQUIDATIONS_PER_MESSAGE {
            NativeParseError::CollectionTooLarge
        } else {
            NativeParseError::InvalidValue
        });
    }
    let topic_symbol = parse_simple_topic(&wire.topic, "allLiquidation.")?;
    let event_time = timestamp(wire.event_time)?;
    let liquidations = wire
        .data
        .into_iter()
        .map(|liquidation| {
            validate_symbol(&liquidation.symbol)?;
            if liquidation.symbol != topic_symbol {
                return Err(NativeParseError::InvalidSymbol);
            }
            Ok(AllLiquidation {
                symbol: liquidation.symbol,
                side: parse_side(&liquidation.side)?,
                quantity: positive_quantity(&liquidation.quantity)?,
                bankruptcy_price: price(&liquidation.bankruptcy_price)?,
                event_time,
                transaction_time: timestamp(liquidation.transaction_time)?,
            })
        })
        .collect::<Result<Vec<_>, NativeParseError>>()?;
    Ok(BybitMessage::AllLiquidations(liquidations))
}

fn parse_orderbook_topic(topic: &str) -> Result<(u32, String), NativeParseError> {
    let mut parts = topic.split('.');
    if parts.next() != Some("orderbook") {
        return Err(NativeParseError::UnsupportedTopic);
    }
    let depth = parts
        .next()
        .and_then(|value| value.parse::<u32>().ok())
        .ok_or(NativeParseError::UnsupportedTopic)?;
    if !matches!(depth, 1 | 50 | 200 | 1_000) {
        return Err(NativeParseError::UnsupportedTopic);
    }
    let symbol = parts.next().ok_or(NativeParseError::UnsupportedTopic)?;
    if parts.next().is_some() {
        return Err(NativeParseError::UnsupportedTopic);
    }
    validate_symbol(symbol)?;
    Ok((depth, symbol.to_owned()))
}

fn parse_simple_topic(topic: &str, prefix: &str) -> Result<String, NativeParseError> {
    let symbol = topic
        .strip_prefix(prefix)
        .ok_or(NativeParseError::UnsupportedTopic)?;
    if symbol.contains('.') {
        return Err(NativeParseError::UnsupportedTopic);
    }
    validate_symbol(symbol)?;
    Ok(symbol.to_owned())
}

pub(crate) fn validate_symbol(symbol: &str) -> Result<(), NativeParseError> {
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

fn validate_trade_id(trade_id: &str) -> Result<(), NativeParseError> {
    if trade_id.is_empty()
        || trade_id.len() > 128
        || trade_id.trim() != trade_id
        || trade_id.chars().any(char::is_control)
    {
        Err(NativeParseError::InvalidValue)
    } else {
        Ok(())
    }
}

fn parse_side(side: &str) -> Result<Side, NativeParseError> {
    match side {
        "Buy" => Ok(Side::Buy),
        "Sell" => Ok(Side::Sell),
        _ => Err(NativeParseError::InvalidValue),
    }
}

fn levels(
    wire: Vec<[String; 2]>,
    kind: BookMessageKind,
) -> Result<Vec<NativeBookLevel>, NativeParseError> {
    wire.into_iter()
        .map(|[price_value, quantity_value]| {
            let quantity = quantity(&quantity_value)?;
            if kind == BookMessageKind::Snapshot && quantity.value().is_zero() {
                return Err(NativeParseError::InvalidValue);
            }
            Ok(NativeBookLevel {
                price: price(&price_value)?,
                quantity,
            })
        })
        .collect()
}

pub(crate) fn timestamp(milliseconds: u64) -> Result<UnixNanos, NativeParseError> {
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

pub(crate) fn price(value: &str) -> Result<Price, NativeParseError> {
    Price::new(decimal(value)?).map_err(|_| NativeParseError::InvalidDecimal)
}

pub(crate) fn quantity(value: &str) -> Result<Quantity, NativeParseError> {
    Quantity::new(decimal(value)?).map_err(|_| NativeParseError::InvalidDecimal)
}

fn positive_quantity(value: &str) -> Result<Quantity, NativeParseError> {
    let quantity = quantity(value)?;
    if quantity.value().is_zero() {
        Err(NativeParseError::InvalidValue)
    } else {
        Ok(quantity)
    }
}

fn rate(value: &str) -> Result<Rate, NativeParseError> {
    Rate::new(decimal(value)?).map_err(|_| NativeParseError::InvalidDecimal)
}

pub(crate) fn ensure_allowed(
    object: &Map<String, Value>,
    allowed: &[&str],
) -> Result<(), NativeParseError> {
    let allowed = allowed.iter().copied().collect::<BTreeSet<_>>();
    if object.keys().any(|key| !allowed.contains(key.as_str())) {
        Err(NativeParseError::SchemaDrift)
    } else {
        Ok(())
    }
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
        let mut values = Vec::with_capacity(sequence.size_hint().unwrap_or(0).min(1_024));
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
struct WireOrderBook {
    topic: String,
    #[serde(rename = "type")]
    message_type: String,
    #[serde(rename = "ts")]
    event_time: u64,
    data: WireOrderBookData,
    #[serde(rename = "cts")]
    matching_engine_time: u64,
}

#[derive(Deserialize)]
struct WireOrderBookData {
    #[serde(rename = "s")]
    symbol: String,
    #[serde(rename = "b")]
    bids: Vec<[String; 2]>,
    #[serde(rename = "a")]
    asks: Vec<[String; 2]>,
    #[serde(rename = "u")]
    update_id: u64,
    #[serde(rename = "seq")]
    cross_sequence: u64,
}

#[derive(Deserialize)]
struct WireTrades {
    topic: String,
    #[serde(rename = "type")]
    message_type: String,
    #[serde(rename = "ts")]
    event_time: u64,
    data: Vec<WireTrade>,
}

#[derive(Deserialize)]
struct WireTrade {
    #[serde(rename = "T")]
    trade_time: u64,
    #[serde(rename = "s")]
    symbol: String,
    #[serde(rename = "S")]
    side: String,
    #[serde(rename = "v")]
    quantity: String,
    #[serde(rename = "p")]
    price: String,
    #[serde(rename = "L")]
    tick_direction: Option<String>,
    #[serde(rename = "i")]
    trade_id: String,
    #[serde(rename = "BT")]
    block_trade: bool,
    #[serde(rename = "RPI")]
    rpi: Option<bool>,
    #[serde(rename = "seq")]
    cross_sequence: u64,
}

#[derive(Deserialize)]
struct WireLinearTicker {
    topic: String,
    #[serde(rename = "type")]
    message_type: String,
    data: WireLinearTickerData,
    #[serde(rename = "cs")]
    cross_sequence: u64,
    #[serde(rename = "ts")]
    event_time: u64,
}

#[derive(Deserialize)]
struct WireLinearTickerData {
    symbol: String,
    #[serde(rename = "markPrice")]
    mark_price: String,
    #[serde(rename = "indexPrice")]
    index_price: String,
    #[serde(rename = "openInterest")]
    open_interest: String,
    #[serde(rename = "openInterestValue")]
    open_interest_value: String,
    #[serde(rename = "fundingRate")]
    funding_rate: String,
    #[serde(rename = "nextFundingTime")]
    next_funding_time: String,
}

#[derive(Deserialize)]
struct WireAllLiquidations {
    topic: String,
    #[serde(rename = "type")]
    message_type: String,
    #[serde(rename = "ts")]
    event_time: u64,
    data: Vec<WireAllLiquidation>,
}

#[derive(Deserialize)]
struct WireAllLiquidation {
    #[serde(rename = "T")]
    transaction_time: u64,
    #[serde(rename = "s")]
    symbol: String,
    #[serde(rename = "S")]
    side: String,
    #[serde(rename = "v")]
    quantity: String,
    #[serde(rename = "p")]
    bankruptcy_price: String,
}
