//! Bounded, source-exact parsing for Kraken public market-data messages.

use std::{collections::BTreeSet, fmt};

use connector_core::DurableRawReference;
use event_envelope::Side;
use fixed_decimal::{FixedDecimal, Price, Quantity};
use serde::de::{MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::value::RawValue;
use serde_json::{Map, Value};
use thiserror::Error;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

pub const MAX_NATIVE_PAYLOAD_BYTES: usize = 1024 * 1024;
const MAX_LEVELS_PER_SIDE: usize = 1_000;
const MAX_L3_ORDERS_PER_MESSAGE: usize = 10_000;
const MAX_TRADES_PER_MESSAGE: usize = 1_024;
const MAX_INSTRUMENT_ASSETS: usize = 10_000;
const MAX_INSTRUMENT_PAIRS: usize = 25_000;
const MAX_DECIMAL_BYTES: usize = 128;
const MAX_IDENTIFIER_BYTES: usize = 192;
const MAX_TIMESTAMP_BYTES: usize = 64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KrakenInput {
    SpotWebSocketV2,
    FuturesWebSocket,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BookMessageKind {
    Snapshot,
    Update,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum L3EventKind {
    Add,
    Modify,
    Delete,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SystemState {
    Online,
    CancelOnly,
    Maintenance,
    PostOnly,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AssetStatus {
    DepositOnly,
    Disabled,
    Enabled,
    FundingTemporarilyDisabled,
    WithdrawalOnly,
    WorkInProgress,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PairStatus {
    CancelOnly,
    Delisted,
    LimitOnly,
    Maintenance,
    Online,
    PostOnly,
    ReduceOnly,
    WorkInProgress,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum KrakenMessage {
    SpotBook(SpotBookMessage),
    SpotLevel3(SpotL3Message),
    SpotTrades {
        kind: BookMessageKind,
        trades: Vec<SpotTrade>,
    },
    SpotStatus {
        state: SystemState,
        connection_id: u64,
    },
    SpotInstrument(SpotInstrumentMessage),
    Heartbeat,
    FuturesBook(FuturesBookMessage),
    FuturesTrades(Vec<FuturesTrade>),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SpotInstrumentMessage {
    pub kind: BookMessageKind,
    pub assets: Vec<SpotAsset>,
    pub pairs: Vec<SpotPair>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SpotAsset {
    pub id: String,
    pub status: AssetStatus,
    pub asset_class: String,
    pub precision: u32,
    pub display_precision: u32,
    pub borrowable: bool,
    pub collateral_value: FixedDecimal,
    pub margin_rate: FixedDecimal,
    pub multiplier: Option<FixedDecimal>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SpotPair {
    pub symbol: String,
    pub base: String,
    pub quote: String,
    pub status: PairStatus,
    pub price_increment: Price,
    pub quantity_increment: Quantity,
    pub minimum_quantity: Quantity,
    pub minimum_cost: FixedDecimal,
    pub cost_precision: u32,
    pub price_precision: u32,
    pub quantity_precision: u32,
    pub display_price_precision: u32,
    pub marginable: bool,
    pub has_index: bool,
    pub initial_margin: Option<FixedDecimal>,
    pub long_position_limit: Option<u64>,
    pub short_position_limit: Option<u64>,
}

/// A parsed Kraken payload bound to one acknowledged raw-WAL record.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DurableKrakenMessage {
    message: KrakenMessage,
    raw_payload_hash: [u8; 32],
}

impl DurableKrakenMessage {
    pub const fn message(&self) -> &KrakenMessage {
        &self.message
    }

    pub const fn raw_payload_hash(&self) -> &[u8; 32] {
        &self.raw_payload_hash
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExactDecimal {
    text: String,
    value: FixedDecimal,
}

impl ExactDecimal {
    pub fn text(&self) -> &str {
        &self.text
    }

    pub const fn value(&self) -> FixedDecimal {
        self.value
    }

    pub fn price(&self) -> Result<Price, NativeParseError> {
        Price::new(self.value).map_err(|_| NativeParseError::InvalidDecimal)
    }

    pub fn quantity(&self) -> Result<Quantity, NativeParseError> {
        Quantity::new(self.value).map_err(|_| NativeParseError::InvalidDecimal)
    }

    fn positive_price(&self) -> Result<Price, NativeParseError> {
        let price = self.price()?;
        if price.value().is_positive() {
            Ok(price)
        } else {
            Err(NativeParseError::InvalidValue)
        }
    }
}

impl<'de> Deserialize<'de> for ExactDecimal {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = Box::<RawValue>::deserialize(deserializer)?;
        let token = raw.get();
        let text = if token.starts_with('"') {
            serde_json::from_str::<String>(token).map_err(serde::de::Error::custom)?
        } else {
            token.to_owned()
        };
        if text.is_empty()
            || text.len() > MAX_DECIMAL_BYTES
            || text.contains(['e', 'E', '+'])
            || text.trim() != text
        {
            return Err(serde::de::Error::custom("invalid exact decimal"));
        }
        let value = FixedDecimal::parse(&text).map_err(serde::de::Error::custom)?;
        Ok(Self { text, value })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeBookLevel {
    pub price: Price,
    pub quantity: Quantity,
    pub price_text: String,
    pub quantity_text: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SpotBookMessage {
    pub symbol: String,
    pub kind: BookMessageKind,
    pub timestamp: String,
    pub checksum: u32,
    pub bids: Vec<NativeBookLevel>,
    pub asks: Vec<NativeBookLevel>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeL3Order {
    pub event: Option<L3EventKind>,
    pub order_id: String,
    pub price: Price,
    pub quantity: Quantity,
    pub price_text: String,
    pub quantity_text: String,
    pub timestamp: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SpotL3Message {
    pub symbol: String,
    pub kind: BookMessageKind,
    pub timestamp: String,
    pub checksum: u32,
    pub bids: Vec<NativeL3Order>,
    pub asks: Vec<NativeL3Order>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SpotTrade {
    pub symbol: String,
    pub side: Side,
    pub quantity: Quantity,
    pub price: Price,
    pub trade_id: u64,
    pub timestamp: String,
    pub order_type: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FuturesBookMessage {
    pub symbol: String,
    pub kind: BookMessageKind,
    pub sequence: u64,
    pub timestamp_ms: u64,
    pub bids: Vec<NativeBookLevel>,
    pub asks: Vec<NativeBookLevel>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FuturesTrade {
    pub symbol: String,
    pub uid: String,
    pub side: Side,
    pub trade_type: String,
    pub sequence: u64,
    pub timestamp_ms: u64,
    pub quantity: Quantity,
    pub price: Price,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum NativeParseError {
    #[error("native payload is empty")]
    EmptyPayload,
    #[error("native payload exceeds the parser bound")]
    PayloadTooLarge,
    #[error("native payload is malformed or contains duplicate object keys")]
    MalformedJson,
    #[error("native payload contains undocumented fields")]
    SchemaDrift,
    #[error("native channel or feed is unsupported")]
    UnsupportedMessage,
    #[error("native payload exceeds a collection bound")]
    Capacity,
    #[error("native payload contains an invalid decimal")]
    InvalidDecimal,
    #[error("native payload contains an invalid value")]
    InvalidValue,
    #[error("native payload does not match its durable raw-WAL record")]
    RawPayloadMismatch,
}

pub fn parse_native_message(
    input: KrakenInput,
    raw: &[u8],
) -> Result<KrakenMessage, NativeParseError> {
    let value = parse_unique_json_bounded(raw)?;
    match input {
        KrakenInput::SpotWebSocketV2 => parse_spot(raw, &value),
        KrakenInput::FuturesWebSocket => parse_futures(raw, &value),
    }
}

pub fn parse_durable_native_message(
    input: KrakenInput,
    raw: &[u8],
    reference: &DurableRawReference,
) -> Result<DurableKrakenMessage, NativeParseError> {
    let raw_payload_hash = *blake3::hash(raw).as_bytes();
    if &raw_payload_hash != reference.payload_hash() {
        return Err(NativeParseError::RawPayloadMismatch);
    }
    Ok(DurableKrakenMessage {
        message: parse_native_message(input, raw)?,
        raw_payload_hash,
    })
}

fn parse_spot(raw: &[u8], value: &Value) -> Result<KrakenMessage, NativeParseError> {
    let channel = value
        .as_object()
        .and_then(|object| object.get("channel"))
        .and_then(Value::as_str)
        .ok_or(NativeParseError::SchemaDrift)?;
    match channel {
        "book" => parse_spot_book(raw),
        "level3" => parse_spot_l3(raw),
        "trade" => parse_spot_trades(raw),
        "instrument" => parse_spot_instrument(raw),
        "status" => parse_spot_status(raw),
        "heartbeat" => {
            let message: WireHeartbeat = strict_decode(raw)?;
            if message.channel == "heartbeat" {
                Ok(KrakenMessage::Heartbeat)
            } else {
                Err(NativeParseError::UnsupportedMessage)
            }
        }
        _ => Err(NativeParseError::UnsupportedMessage),
    }
}

fn parse_spot_instrument(raw: &[u8]) -> Result<KrakenMessage, NativeParseError> {
    let wire: WireSpotInstrumentEnvelope = strict_decode(raw)?;
    if wire.channel != "instrument"
        || wire.data.assets.len() > MAX_INSTRUMENT_ASSETS
        || wire.data.pairs.len() > MAX_INSTRUMENT_PAIRS
        || wire.data.assets.is_empty() && wire.data.pairs.is_empty()
    {
        return Err(NativeParseError::Capacity);
    }
    let kind = parse_message_kind(&wire.message_type)?;
    let mut asset_ids = BTreeSet::new();
    let mut assets = Vec::with_capacity(wire.data.assets.len());
    for asset in wire.data.assets {
        validate_identifier(&asset.id)?;
        validate_identifier(&asset.asset_class)?;
        if !asset_ids.insert(asset.id.clone())
            || asset.precision > MAX_DECIMAL_BYTES as u32
            || asset.precision_display > MAX_DECIMAL_BYTES as u32
            || asset.collateral_value.value().is_negative()
            || asset.margin_rate.value().is_negative()
            || asset
                .multiplier
                .as_ref()
                .is_some_and(|value| !value.value().is_positive())
        {
            return Err(NativeParseError::InvalidValue);
        }
        assets.push(SpotAsset {
            id: asset.id,
            status: parse_asset_status(&asset.status)?,
            asset_class: asset.asset_class,
            precision: asset.precision,
            display_precision: asset.precision_display,
            borrowable: asset.borrowable,
            collateral_value: asset.collateral_value.value(),
            margin_rate: asset.margin_rate.value(),
            multiplier: asset.multiplier.map(|value| value.value()),
        });
    }
    let mut pair_symbols = BTreeSet::new();
    let mut pairs = Vec::with_capacity(wire.data.pairs.len());
    for pair in wire.data.pairs {
        validate_identifier(&pair.symbol)?;
        validate_identifier(&pair.base)?;
        validate_identifier(&pair.quote)?;
        if !pair_symbols.insert(pair.symbol.clone())
            || kind == BookMessageKind::Snapshot
                && (!asset_ids.contains(&pair.base) || !asset_ids.contains(&pair.quote))
            || pair.cost_precision > MAX_DECIMAL_BYTES as u32
            || pair.price_precision > MAX_DECIMAL_BYTES as u32
            || pair.qty_precision > MAX_DECIMAL_BYTES as u32
            || pair.ws_display_price_precision > MAX_DECIMAL_BYTES as u32
            || !pair.cost_min.value().is_positive()
            || pair
                .margin_initial
                .as_ref()
                .is_some_and(|value| value.value().is_negative())
        {
            return Err(NativeParseError::InvalidValue);
        }
        let price_increment = pair.price_increment.positive_price()?;
        if pair
            .tick_size
            .as_ref()
            .is_some_and(|deprecated| deprecated.value() != price_increment.value())
        {
            return Err(NativeParseError::InvalidValue);
        }
        pairs.push(SpotPair {
            symbol: pair.symbol,
            base: pair.base,
            quote: pair.quote,
            status: parse_pair_status(&pair.status)?,
            price_increment,
            quantity_increment: positive_quantity(&pair.qty_increment)?,
            minimum_quantity: positive_quantity(&pair.qty_min)?,
            minimum_cost: pair.cost_min.value(),
            cost_precision: pair.cost_precision,
            price_precision: pair.price_precision,
            quantity_precision: pair.qty_precision,
            display_price_precision: pair.ws_display_price_precision,
            marginable: pair.marginable,
            has_index: pair.has_index,
            initial_margin: pair.margin_initial.map(|value| value.value()),
            long_position_limit: pair.position_limit_long,
            short_position_limit: pair.position_limit_short,
        });
    }
    Ok(KrakenMessage::SpotInstrument(SpotInstrumentMessage {
        kind,
        assets,
        pairs,
    }))
}

fn parse_spot_book(raw: &[u8]) -> Result<KrakenMessage, NativeParseError> {
    let wire: WireSpotBookEnvelope = strict_decode(raw)?;
    if wire.channel != "book" || wire.data.len() != 1 {
        return Err(NativeParseError::InvalidValue);
    }
    let kind = parse_message_kind(&wire.message_type)?;
    let data = wire
        .data
        .into_iter()
        .next()
        .ok_or(NativeParseError::InvalidValue)?;
    validate_identifier(&data.symbol)?;
    validate_timestamp(&data.timestamp)?;
    let bids = levels(
        data.bids,
        MAX_LEVELS_PER_SIDE,
        kind == BookMessageKind::Update,
    )?;
    let asks = levels(
        data.asks,
        MAX_LEVELS_PER_SIDE,
        kind == BookMessageKind::Update,
    )?;
    if bids.is_empty() && asks.is_empty() {
        return Err(NativeParseError::InvalidValue);
    }
    Ok(KrakenMessage::SpotBook(SpotBookMessage {
        symbol: data.symbol,
        kind,
        timestamp: data.timestamp,
        checksum: data.checksum,
        bids,
        asks,
    }))
}

fn parse_spot_l3(raw: &[u8]) -> Result<KrakenMessage, NativeParseError> {
    let wire: WireSpotL3Envelope = strict_decode(raw)?;
    if wire.channel != "level3" || wire.data.len() != 1 {
        return Err(NativeParseError::InvalidValue);
    }
    let kind = parse_message_kind(&wire.message_type)?;
    let data = wire
        .data
        .into_iter()
        .next()
        .ok_or(NativeParseError::InvalidValue)?;
    validate_identifier(&data.symbol)?;
    validate_timestamp(&data.timestamp)?;
    let bids = l3_orders(data.bids, kind)?;
    let asks = l3_orders(data.asks, kind)?;
    if bids.is_empty() && asks.is_empty() {
        return Err(NativeParseError::InvalidValue);
    }
    Ok(KrakenMessage::SpotLevel3(SpotL3Message {
        symbol: data.symbol,
        kind,
        timestamp: data.timestamp,
        checksum: data.checksum,
        bids,
        asks,
    }))
}

fn parse_spot_trades(raw: &[u8]) -> Result<KrakenMessage, NativeParseError> {
    let wire: WireSpotTradeEnvelope = strict_decode(raw)?;
    if wire.channel != "trade" || wire.data.is_empty() || wire.data.len() > MAX_TRADES_PER_MESSAGE {
        return Err(NativeParseError::Capacity);
    }
    let kind = parse_message_kind(&wire.message_type)?;
    let mut trades = Vec::with_capacity(wire.data.len());
    for trade in wire.data {
        validate_identifier(&trade.symbol)?;
        validate_timestamp(&trade.timestamp)?;
        validate_identifier(&trade.ord_type)?;
        trades.push(SpotTrade {
            symbol: trade.symbol,
            side: parse_side(&trade.side)?,
            quantity: positive_quantity(&trade.qty)?,
            price: trade.price.positive_price()?,
            trade_id: trade.trade_id,
            timestamp: trade.timestamp,
            order_type: trade.ord_type,
        });
    }
    Ok(KrakenMessage::SpotTrades { kind, trades })
}

fn parse_spot_status(raw: &[u8]) -> Result<KrakenMessage, NativeParseError> {
    let wire: WireSpotStatusEnvelope = strict_decode(raw)?;
    if wire.channel != "status" || wire.message_type != "update" || wire.data.len() != 1 {
        return Err(NativeParseError::InvalidValue);
    }
    let status = wire
        .data
        .into_iter()
        .next()
        .ok_or(NativeParseError::InvalidValue)?;
    validate_identifier(&status.version)?;
    validate_identifier(&status.api_version)?;
    let state = match status.system.as_str() {
        "online" => SystemState::Online,
        "cancel_only" => SystemState::CancelOnly,
        "maintenance" => SystemState::Maintenance,
        "post_only" => SystemState::PostOnly,
        _ => return Err(NativeParseError::InvalidValue),
    };
    Ok(KrakenMessage::SpotStatus {
        state,
        connection_id: status.connection_id,
    })
}

fn parse_futures(raw: &[u8], value: &Value) -> Result<KrakenMessage, NativeParseError> {
    let feed = value
        .as_object()
        .and_then(|object| object.get("feed"))
        .and_then(Value::as_str)
        .ok_or(NativeParseError::SchemaDrift)?;
    match feed {
        "book_snapshot" => {
            let wire: WireFuturesBookSnapshot = strict_decode(raw)?;
            if wire.feed != "book_snapshot" {
                return Err(NativeParseError::InvalidValue);
            }
            validate_identifier(&wire.product_id)?;
            let bids = futures_levels(wire.bids, false)?;
            let asks = futures_levels(wire.asks, false)?;
            if wire.sequence == 0 || wire.timestamp == 0 || bids.is_empty() && asks.is_empty() {
                return Err(NativeParseError::InvalidValue);
            }
            Ok(KrakenMessage::FuturesBook(FuturesBookMessage {
                symbol: wire.product_id,
                kind: BookMessageKind::Snapshot,
                sequence: wire.sequence,
                timestamp_ms: wire.timestamp,
                bids,
                asks,
            }))
        }
        "book" => {
            let wire: WireFuturesBookDelta = strict_decode(raw)?;
            if wire.feed != "book" {
                return Err(NativeParseError::InvalidValue);
            }
            validate_identifier(&wire.product_id)?;
            if wire.sequence == 0 || wire.timestamp == 0 {
                return Err(NativeParseError::InvalidValue);
            }
            let level = native_level(wire.price, wire.qty, true)?;
            let (bids, asks) = match wire.side.as_str() {
                "buy" => (vec![level], Vec::new()),
                "sell" => (Vec::new(), vec![level]),
                _ => return Err(NativeParseError::InvalidValue),
            };
            Ok(KrakenMessage::FuturesBook(FuturesBookMessage {
                symbol: wire.product_id,
                kind: BookMessageKind::Update,
                sequence: wire.sequence,
                timestamp_ms: wire.timestamp,
                bids,
                asks,
            }))
        }
        "trade" => {
            let wire: WireFuturesTrade = strict_decode(raw)?;
            Ok(KrakenMessage::FuturesTrades(vec![futures_trade(wire)?]))
        }
        "trade_snapshot" => {
            let wire: WireFuturesTradeSnapshot = strict_decode(raw)?;
            if wire.feed != "trade_snapshot"
                || wire.trades.is_empty()
                || wire.trades.len() > MAX_TRADES_PER_MESSAGE
            {
                return Err(NativeParseError::Capacity);
            }
            validate_identifier(&wire.product_id)?;
            let mut trades = Vec::with_capacity(wire.trades.len());
            for trade in wire.trades {
                if trade.product_id != wire.product_id {
                    return Err(NativeParseError::InvalidValue);
                }
                trades.push(futures_trade(trade)?);
            }
            Ok(KrakenMessage::FuturesTrades(trades))
        }
        _ => Err(NativeParseError::UnsupportedMessage),
    }
}

fn futures_trade(wire: WireFuturesTrade) -> Result<FuturesTrade, NativeParseError> {
    if wire.feed != "trade" {
        return Err(NativeParseError::InvalidValue);
    }
    validate_identifier(&wire.product_id)?;
    validate_identifier(&wire.uid)?;
    validate_identifier(&wire.trade_type)?;
    if wire.sequence == 0 || wire.timestamp == 0 {
        return Err(NativeParseError::InvalidValue);
    }
    Ok(FuturesTrade {
        symbol: wire.product_id,
        uid: wire.uid,
        side: parse_side(&wire.side)?,
        trade_type: wire.trade_type,
        sequence: wire.sequence,
        timestamp_ms: wire.timestamp,
        quantity: positive_quantity(&wire.qty)?,
        price: wire.price.positive_price()?,
    })
}

fn levels(
    wire: Vec<WireLevel>,
    maximum: usize,
    allow_zero: bool,
) -> Result<Vec<NativeBookLevel>, NativeParseError> {
    if wire.len() > maximum {
        return Err(NativeParseError::Capacity);
    }
    wire.into_iter()
        .map(|level| native_level(level.price, level.qty, allow_zero))
        .collect()
}

fn futures_levels(
    wire: Vec<WireFuturesLevel>,
    allow_zero: bool,
) -> Result<Vec<NativeBookLevel>, NativeParseError> {
    if wire.len() > MAX_LEVELS_PER_SIDE {
        return Err(NativeParseError::Capacity);
    }
    wire.into_iter()
        .map(|level| native_level(level.price, level.qty, allow_zero))
        .collect()
}

fn native_level(
    price: ExactDecimal,
    quantity: ExactDecimal,
    allow_zero: bool,
) -> Result<NativeBookLevel, NativeParseError> {
    let parsed_quantity = quantity.quantity()?;
    if !allow_zero && parsed_quantity.value().is_zero() {
        return Err(NativeParseError::InvalidValue);
    }
    Ok(NativeBookLevel {
        price: price.positive_price()?,
        quantity: parsed_quantity,
        price_text: price.text,
        quantity_text: quantity.text,
    })
}

fn l3_orders(
    wire: Vec<WireL3Order>,
    kind: BookMessageKind,
) -> Result<Vec<NativeL3Order>, NativeParseError> {
    if wire.len() > MAX_L3_ORDERS_PER_MESSAGE {
        return Err(NativeParseError::Capacity);
    }
    wire.into_iter()
        .map(|order| {
            validate_identifier(&order.order_id)?;
            validate_timestamp(&order.timestamp)?;
            let event = match (kind, order.event.as_deref()) {
                (BookMessageKind::Snapshot, None) => None,
                (BookMessageKind::Update, Some("add")) => Some(L3EventKind::Add),
                (BookMessageKind::Update, Some("modify")) => Some(L3EventKind::Modify),
                (BookMessageKind::Update, Some("delete")) => Some(L3EventKind::Delete),
                _ => return Err(NativeParseError::InvalidValue),
            };
            let quantity = order.order_qty.quantity()?;
            if event != Some(L3EventKind::Delete) && quantity.value().is_zero() {
                return Err(NativeParseError::InvalidValue);
            }
            Ok(NativeL3Order {
                event,
                order_id: order.order_id,
                price: order.limit_price.positive_price()?,
                quantity,
                price_text: order.limit_price.text,
                quantity_text: order.order_qty.text,
                timestamp: order.timestamp,
            })
        })
        .collect()
}

fn positive_quantity(value: &ExactDecimal) -> Result<Quantity, NativeParseError> {
    let quantity = value.quantity()?;
    if quantity.value().is_zero() {
        Err(NativeParseError::InvalidValue)
    } else {
        Ok(quantity)
    }
}

fn parse_message_kind(value: &str) -> Result<BookMessageKind, NativeParseError> {
    match value {
        "snapshot" => Ok(BookMessageKind::Snapshot),
        "update" => Ok(BookMessageKind::Update),
        _ => Err(NativeParseError::InvalidValue),
    }
}

fn parse_side(value: &str) -> Result<Side, NativeParseError> {
    match value {
        "buy" => Ok(Side::Buy),
        "sell" => Ok(Side::Sell),
        _ => Err(NativeParseError::InvalidValue),
    }
}

fn parse_asset_status(value: &str) -> Result<AssetStatus, NativeParseError> {
    match value {
        "depositonly" => Ok(AssetStatus::DepositOnly),
        "disabled" => Ok(AssetStatus::Disabled),
        "enabled" => Ok(AssetStatus::Enabled),
        "fundingtemporarilydisabled" => Ok(AssetStatus::FundingTemporarilyDisabled),
        "withdrawalonly" => Ok(AssetStatus::WithdrawalOnly),
        "workinprogress" => Ok(AssetStatus::WorkInProgress),
        _ => Err(NativeParseError::InvalidValue),
    }
}

fn parse_pair_status(value: &str) -> Result<PairStatus, NativeParseError> {
    match value {
        "cancel_only" => Ok(PairStatus::CancelOnly),
        "delisted" => Ok(PairStatus::Delisted),
        "limit_only" => Ok(PairStatus::LimitOnly),
        "maintenance" => Ok(PairStatus::Maintenance),
        "online" => Ok(PairStatus::Online),
        "post_only" => Ok(PairStatus::PostOnly),
        "reduce_only" => Ok(PairStatus::ReduceOnly),
        "work_in_progress" => Ok(PairStatus::WorkInProgress),
        _ => Err(NativeParseError::InvalidValue),
    }
}

fn validate_identifier(value: &str) -> Result<(), NativeParseError> {
    if value.is_empty()
        || value.len() > MAX_IDENTIFIER_BYTES
        || value.trim() != value
        || !value.is_ascii()
        || value.chars().any(char::is_control)
    {
        Err(NativeParseError::InvalidValue)
    } else {
        Ok(())
    }
}

fn validate_timestamp(value: &str) -> Result<(), NativeParseError> {
    if value.len() < 20
        || value.len() > MAX_TIMESTAMP_BYTES
        || !value.is_ascii()
        || !value.ends_with('Z')
        || OffsetDateTime::parse(value, &Rfc3339).is_err()
    {
        Err(NativeParseError::InvalidValue)
    } else {
        Ok(())
    }
}

fn strict_decode<'a, T: Deserialize<'a>>(raw: &'a [u8]) -> Result<T, NativeParseError> {
    serde_json::from_slice(raw).map_err(|_| NativeParseError::SchemaDrift)
}

fn parse_unique_json_bounded(raw: &[u8]) -> Result<Value, NativeParseError> {
    if raw.len() > MAX_NATIVE_PAYLOAD_BYTES {
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
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(UniqueValueVisitor)
    }
}

struct UniqueValueVisitor;

impl<'de> Visitor<'de> for UniqueValueVisitor {
    type Value = UniqueValue;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("JSON without duplicate object keys")
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
#[serde(deny_unknown_fields)]
struct WireSpotBookEnvelope {
    channel: String,
    #[serde(rename = "type")]
    message_type: String,
    data: Vec<WireSpotBookData>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireSpotBookData {
    symbol: String,
    bids: Vec<WireLevel>,
    asks: Vec<WireLevel>,
    checksum: u32,
    timestamp: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireLevel {
    price: ExactDecimal,
    qty: ExactDecimal,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireSpotL3Envelope {
    channel: String,
    #[serde(rename = "type")]
    message_type: String,
    data: Vec<WireSpotL3Data>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireSpotL3Data {
    symbol: String,
    bids: Vec<WireL3Order>,
    asks: Vec<WireL3Order>,
    checksum: u32,
    timestamp: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireL3Order {
    event: Option<String>,
    order_id: String,
    limit_price: ExactDecimal,
    order_qty: ExactDecimal,
    timestamp: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireSpotTradeEnvelope {
    channel: String,
    #[serde(rename = "type")]
    message_type: String,
    data: Vec<WireSpotTrade>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireSpotTrade {
    symbol: String,
    side: String,
    qty: ExactDecimal,
    price: ExactDecimal,
    ord_type: String,
    trade_id: u64,
    timestamp: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireSpotInstrumentEnvelope {
    channel: String,
    #[serde(rename = "type")]
    message_type: String,
    data: WireSpotInstrumentData,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireSpotInstrumentData {
    assets: Vec<WireSpotAsset>,
    pairs: Vec<WireSpotPair>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireSpotAsset {
    borrowable: bool,
    collateral_value: ExactDecimal,
    id: String,
    margin_rate: ExactDecimal,
    precision: u32,
    precision_display: u32,
    multiplier: Option<ExactDecimal>,
    #[serde(rename = "class")]
    asset_class: String,
    status: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireSpotPair {
    base: String,
    quote: String,
    cost_min: ExactDecimal,
    cost_precision: u32,
    has_index: bool,
    margin_initial: Option<ExactDecimal>,
    marginable: bool,
    position_limit_long: Option<u64>,
    position_limit_short: Option<u64>,
    price_increment: ExactDecimal,
    price_precision: u32,
    qty_increment: ExactDecimal,
    qty_min: ExactDecimal,
    qty_precision: u32,
    ws_display_price_precision: u32,
    status: String,
    symbol: String,
    tick_size: Option<ExactDecimal>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireSpotStatusEnvelope {
    channel: String,
    #[serde(rename = "type")]
    message_type: String,
    data: Vec<WireSpotStatus>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireSpotStatus {
    system: String,
    version: String,
    api_version: String,
    connection_id: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireHeartbeat {
    channel: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireFuturesBookSnapshot {
    feed: String,
    product_id: String,
    #[serde(rename = "seq")]
    sequence: u64,
    timestamp: u64,
    #[serde(rename = "tickSize")]
    _tick_size: Option<String>,
    bids: Vec<WireFuturesLevel>,
    asks: Vec<WireFuturesLevel>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireFuturesLevel {
    qty: ExactDecimal,
    price: ExactDecimal,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireFuturesBookDelta {
    feed: String,
    product_id: String,
    side: String,
    #[serde(rename = "seq")]
    sequence: u64,
    price: ExactDecimal,
    qty: ExactDecimal,
    timestamp: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireFuturesTradeSnapshot {
    feed: String,
    product_id: String,
    trades: Vec<WireFuturesTrade>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireFuturesTrade {
    feed: String,
    product_id: String,
    uid: String,
    side: String,
    #[serde(rename = "type")]
    trade_type: String,
    #[serde(rename = "seq")]
    sequence: u64,
    #[serde(rename = "time")]
    timestamp: u64,
    qty: ExactDecimal,
    price: ExactDecimal,
}
