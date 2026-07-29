//! Bounded, source-exact parsing for Deribit JSON-RPC market data.

use std::{collections::BTreeSet, fmt};

use connector_core::DurableRawReference;
use domain::UnixNanos;
use event_envelope::Side;
use fixed_decimal::{FixedDecimal, MAX_SCALE, Price, Quantity};
use serde::de::{MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::value::RawValue;
use serde_json::{Map, Value};
use thiserror::Error;

pub const MAX_NATIVE_PAYLOAD_BYTES: usize = 1024 * 1024;
const MAX_DECIMAL_BYTES: usize = 128;
const MAX_IDENTIFIER_BYTES: usize = 192;
const MAX_LEVELS_PER_SIDE: usize = 10_000;
const MAX_TRADES_PER_MESSAGE: usize = 4_096;
const MAX_INSTRUMENTS_PER_RESPONSE: usize = 25_000;
const MAX_DELIVERY_RECORDS: usize = 10_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeribitInput {
    Subscription,
    InstrumentsResponse,
    DeliveryPricesResponse,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BookMessageKind {
    Snapshot,
    Change,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BookAction {
    New,
    Change,
    Delete,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeribitInstrumentKind {
    Future,
    Option,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeribitInstrumentType {
    Linear,
    Reversed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OptionKind {
    Call,
    Put,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeribitLifecycle {
    Open,
    Settlement,
    Delivered,
    Inactive,
    Locked,
    Halted,
    Archivized,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DeribitMessage {
    Book(DeribitBookMessage),
    Trades(Vec<DeribitTrade>),
    Ticker(Box<DeribitTicker>),
    PerpetualInterest(DeribitPerpetualInterest),
    Instruments(Vec<DeribitInstrument>),
    DeliveryPrices {
        records: Vec<DeribitDeliveryPrice>,
        records_total: u64,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DurableDeribitMessage {
    message: DeribitMessage,
    raw_payload_hash: [u8; 32],
}

impl DurableDeribitMessage {
    pub const fn message(&self) -> &DeribitMessage {
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
        let value = self.price()?;
        if value.value().is_positive() {
            Ok(value)
        } else {
            Err(NativeParseError::InvalidValue)
        }
    }

    fn positive_quantity(&self) -> Result<Quantity, NativeParseError> {
        let value = self.quantity()?;
        if value.value().is_positive() {
            Ok(value)
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
        if text.is_empty() || text.len() > MAX_DECIMAL_BYTES || text.trim() != text {
            return Err(serde::de::Error::custom("invalid exact decimal"));
        }
        let value = parse_scientific_decimal(&text).map_err(serde::de::Error::custom)?;
        Ok(Self { text, value })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeBookLevel {
    pub action: BookAction,
    pub price: Price,
    pub quantity: Quantity,
    pub price_text: String,
    pub quantity_text: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeribitBookMessage {
    pub instrument_name: String,
    pub kind: BookMessageKind,
    pub timestamp: UnixNanos,
    pub change_id: u64,
    pub previous_change_id: Option<u64>,
    pub bids: Vec<NativeBookLevel>,
    pub asks: Vec<NativeBookLevel>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeribitTrade {
    pub instrument_name: String,
    pub trade_id: String,
    pub trade_sequence: u64,
    pub timestamp: UnixNanos,
    pub starbase_timestamp: Option<UnixNanos>,
    pub direction: Side,
    pub price: Price,
    pub amount: Quantity,
    pub contracts: Option<Quantity>,
    pub mark_price: Price,
    pub index_price: Price,
    pub implied_volatility: Option<FixedDecimal>,
    pub tick_direction: u8,
    pub liquidation: Option<String>,
    pub block_trade_id: Option<String>,
    pub block_trade_leg_count: Option<u64>,
    pub combo_id: Option<String>,
    pub combo_trade_id: Option<String>,
    pub starbase_match_id: Option<u64>,
    pub block_rfq_id: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeribitTicker {
    pub instrument_name: String,
    pub timestamp: UnixNanos,
    pub lifecycle: DeribitLifecycle,
    pub stats: DeribitTickerStats,
    pub index_price: Price,
    pub minimum_price: Price,
    pub maximum_price: Price,
    pub mark_price: Price,
    pub estimated_delivery_price: Price,
    pub last_price: Option<Price>,
    pub best_bid_price: Option<Price>,
    pub best_bid_amount: Quantity,
    pub best_ask_price: Option<Price>,
    pub best_ask_amount: Quantity,
    pub open_interest: Quantity,
    pub settlement_price: Option<Price>,
    pub delivery_price: Option<Price>,
    pub current_funding: Option<FixedDecimal>,
    pub funding_8h: Option<FixedDecimal>,
    pub interest_value: Option<FixedDecimal>,
    pub underlying_price: Option<Price>,
    pub underlying_index: Option<String>,
    pub interest_rate: Option<FixedDecimal>,
    pub bid_iv: Option<FixedDecimal>,
    pub ask_iv: Option<FixedDecimal>,
    pub mark_iv: Option<FixedDecimal>,
    pub greeks: Option<DeribitGreeks>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeribitTickerStats {
    pub volume: Quantity,
    pub low: Option<Price>,
    pub high: Option<Price>,
    pub price_change_percent: Option<FixedDecimal>,
    pub volume_usd: Option<Quantity>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeribitGreeks {
    pub delta: FixedDecimal,
    pub gamma: FixedDecimal,
    pub rho: FixedDecimal,
    pub theta: FixedDecimal,
    pub vega: FixedDecimal,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeribitPerpetualInterest {
    pub instrument_name: String,
    pub timestamp: UnixNanos,
    pub interest: FixedDecimal,
    pub index_price: Price,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeribitInstrument {
    pub instrument_name: String,
    pub instrument_id: u64,
    pub kind: DeribitInstrumentKind,
    pub instrument_type: DeribitInstrumentType,
    pub settlement_period: String,
    pub price_index: String,
    pub underlying_type: String,
    pub active: bool,
    pub lifecycle: DeribitLifecycle,
    pub base_currency: String,
    pub quote_currency: String,
    pub counter_currency: Option<String>,
    pub settlement_currency: String,
    pub contract_size: FixedDecimal,
    pub price_tick: Price,
    pub quantity_step: Quantity,
    pub minimum_trade_amount: Quantity,
    pub creation_time: UnixNanos,
    pub expiration_time: Option<UnixNanos>,
    pub strike: Option<Price>,
    pub option_kind: Option<OptionKind>,
}

impl DeribitInstrument {
    pub fn is_perpetual(&self) -> bool {
        self.kind == DeribitInstrumentKind::Future && self.settlement_period == "perpetual"
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeribitDeliveryPrice {
    pub date: String,
    pub delivery_price: Price,
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
    #[error("native channel or method is unsupported")]
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
    input: DeribitInput,
    raw: &[u8],
) -> Result<DeribitMessage, NativeParseError> {
    let value = parse_unique_json_bounded(raw)?;
    match input {
        DeribitInput::Subscription => parse_subscription(raw, &value),
        DeribitInput::InstrumentsResponse => parse_instruments_response(raw),
        DeribitInput::DeliveryPricesResponse => parse_delivery_response(raw),
    }
}

pub fn parse_durable_native_message(
    input: DeribitInput,
    raw: &[u8],
    reference: &DurableRawReference,
) -> Result<DurableDeribitMessage, NativeParseError> {
    let raw_payload_hash = *blake3::hash(raw).as_bytes();
    if &raw_payload_hash != reference.payload_hash() {
        return Err(NativeParseError::RawPayloadMismatch);
    }
    Ok(DurableDeribitMessage {
        message: parse_native_message(input, raw)?,
        raw_payload_hash,
    })
}

fn parse_subscription(raw: &[u8], value: &Value) -> Result<DeribitMessage, NativeParseError> {
    let root = value.as_object().ok_or(NativeParseError::MalformedJson)?;
    let channel = root
        .get("params")
        .and_then(Value::as_object)
        .and_then(|params| params.get("channel"))
        .and_then(Value::as_str)
        .ok_or(NativeParseError::SchemaDrift)?;
    if channel.starts_with("book.") {
        parse_book(raw)
    } else if channel.starts_with("trades.") {
        parse_trades(raw)
    } else if channel.starts_with("ticker.") {
        parse_ticker(raw)
    } else if channel.starts_with("perpetual.") {
        parse_perpetual_interest(raw)
    } else {
        Err(NativeParseError::UnsupportedMessage)
    }
}

fn parse_book(raw: &[u8]) -> Result<DeribitMessage, NativeParseError> {
    let envelope: WireSubscriptionEnvelope = strict_decode(raw)?;
    validate_envelope(&envelope)?;
    let data: WireBook = strict_decode(envelope.params.data.get().as_bytes())?;
    let instrument = validate_channel(&envelope.params.channel, "book", true)?;
    if instrument != data.instrument_name {
        return Err(NativeParseError::InvalidValue);
    }
    let kind = match data.message_type.as_str() {
        "snapshot" if data.prev_change_id.is_none() => BookMessageKind::Snapshot,
        "change" if data.prev_change_id.is_some() => BookMessageKind::Change,
        _ => return Err(NativeParseError::InvalidValue),
    };
    if data.change_id == 0
        || data.prev_change_id == Some(0)
        || data.bids.len() > MAX_LEVELS_PER_SIDE
        || data.asks.len() > MAX_LEVELS_PER_SIDE
        || data.bids.is_empty() && data.asks.is_empty()
    {
        return Err(NativeParseError::Capacity);
    }
    Ok(DeribitMessage::Book(DeribitBookMessage {
        instrument_name: data.instrument_name,
        kind,
        timestamp: millis_to_nanos(data.timestamp)?,
        change_id: data.change_id,
        previous_change_id: data.prev_change_id,
        bids: parse_levels(data.bids)?,
        asks: parse_levels(data.asks)?,
    }))
}

fn parse_trades(raw: &[u8]) -> Result<DeribitMessage, NativeParseError> {
    let envelope: WireSubscriptionEnvelope = strict_decode(raw)?;
    validate_envelope(&envelope)?;
    let instrument = validate_channel(&envelope.params.channel, "trades", true)?;
    let data: Vec<WireTrade> = strict_decode(envelope.params.data.get().as_bytes())?;
    if data.is_empty() || data.len() > MAX_TRADES_PER_MESSAGE {
        return Err(NativeParseError::Capacity);
    }
    let mut ids = BTreeSet::new();
    let mut sequences = BTreeSet::new();
    let mut trades = Vec::with_capacity(data.len());
    for trade in data {
        validate_identifier(&trade.instrument_name)?;
        validate_identifier(&trade.trade_id)?;
        if trade.instrument_name != instrument
            || trade.trade_seq == 0
            || !ids.insert(trade.trade_id.clone())
            || !sequences.insert(trade.trade_seq)
            || trade.tick_direction > 3
            || trade.block_trade_leg_count == Some(0)
            || trade.starbase_match_id == Some(0)
            || trade.block_rfq_id == Some(0)
        {
            return Err(NativeParseError::InvalidValue);
        }
        for identifier in [
            trade.block_trade_id.as_deref(),
            trade.combo_id.as_deref(),
            trade.combo_trade_id.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            validate_identifier(identifier)?;
        }
        let liquidation = match trade.liquidation.as_deref() {
            None | Some("M") | Some("T") | Some("MT") => trade.liquidation,
            Some(_) => return Err(NativeParseError::InvalidValue),
        };
        trades.push(DeribitTrade {
            instrument_name: trade.instrument_name,
            trade_id: trade.trade_id,
            trade_sequence: trade.trade_seq,
            timestamp: millis_to_nanos(trade.timestamp)?,
            starbase_timestamp: trade.starbase_timestamp.map(nanos_from_u64).transpose()?,
            direction: parse_side(&trade.direction)?,
            price: trade.price.positive_price()?,
            amount: trade.amount.positive_quantity()?,
            contracts: trade
                .contracts
                .as_ref()
                .map(ExactDecimal::positive_quantity)
                .transpose()?,
            mark_price: trade.mark_price.positive_price()?,
            index_price: trade.index_price.positive_price()?,
            implied_volatility: optional_nonnegative_decimal(trade.iv)?,
            tick_direction: trade.tick_direction,
            liquidation,
            block_trade_id: trade.block_trade_id,
            block_trade_leg_count: trade.block_trade_leg_count,
            combo_id: trade.combo_id,
            combo_trade_id: trade.combo_trade_id,
            starbase_match_id: trade.starbase_match_id,
            block_rfq_id: trade.block_rfq_id,
        });
    }
    Ok(DeribitMessage::Trades(trades))
}

fn parse_ticker(raw: &[u8]) -> Result<DeribitMessage, NativeParseError> {
    let envelope: WireSubscriptionEnvelope = strict_decode(raw)?;
    validate_envelope(&envelope)?;
    let instrument = validate_channel(&envelope.params.channel, "ticker", true)?;
    let data: WireTicker = strict_decode(envelope.params.data.get().as_bytes())?;
    if data.instrument_name != instrument {
        return Err(NativeParseError::InvalidValue);
    }
    let greeks = data.greeks.map(|value| DeribitGreeks {
        delta: value.delta.value(),
        gamma: value.gamma.value(),
        rho: value.rho.value(),
        theta: value.theta.value(),
        vega: value.vega.value(),
    });
    let ticker = DeribitTicker {
        instrument_name: data.instrument_name,
        timestamp: millis_to_nanos(data.timestamp)?,
        lifecycle: parse_lifecycle(&data.state)?,
        stats: DeribitTickerStats {
            volume: nonnegative_quantity(&data.stats.volume)?,
            low: optional_positive_price(data.stats.low.0.as_ref())?,
            high: optional_positive_price(data.stats.high.0.as_ref())?,
            price_change_percent: data.stats.price_change.0.map(|value| value.value()),
            volume_usd: optional_nonnegative_quantity(data.stats.volume_usd.as_ref())?,
        },
        index_price: data.index_price.positive_price()?,
        minimum_price: data.min_price.positive_price()?,
        maximum_price: data.max_price.positive_price()?,
        mark_price: data.mark_price.positive_price()?,
        estimated_delivery_price: data.estimated_delivery_price.positive_price()?,
        last_price: optional_positive_price(data.last_price.0.as_ref())?,
        best_bid_price: optional_positive_price(data.best_bid_price.0.as_ref())?,
        best_bid_amount: nonnegative_quantity(&data.best_bid_amount)?,
        best_ask_price: optional_positive_price(data.best_ask_price.0.as_ref())?,
        best_ask_amount: nonnegative_quantity(&data.best_ask_amount)?,
        open_interest: data.open_interest.quantity()?,
        settlement_price: optional_positive_price(data.settlement_price.as_ref())?,
        delivery_price: optional_positive_price(data.delivery_price.as_ref())?,
        current_funding: optional_decimal(data.current_funding),
        funding_8h: optional_decimal(data.funding_8h),
        interest_value: optional_decimal(data.interest_value),
        underlying_price: optional_positive_price(data.underlying_price.as_ref())?,
        underlying_index: data.underlying_index,
        interest_rate: optional_decimal(data.interest_rate),
        bid_iv: optional_nonnegative_decimal(data.bid_iv)?,
        ask_iv: optional_nonnegative_decimal(data.ask_iv)?,
        mark_iv: optional_nonnegative_decimal(data.mark_iv)?,
        greeks,
    };
    validate_ticker_product_fields(&ticker)?;
    Ok(DeribitMessage::Ticker(Box::new(ticker)))
}

fn parse_perpetual_interest(raw: &[u8]) -> Result<DeribitMessage, NativeParseError> {
    let envelope: WireSubscriptionEnvelope = strict_decode(raw)?;
    validate_envelope(&envelope)?;
    let instrument = validate_channel(&envelope.params.channel, "perpetual", true)?;
    if !instrument.ends_with("-PERPETUAL") {
        return Err(NativeParseError::InvalidValue);
    }
    let data: WirePerpetual = strict_decode(envelope.params.data.get().as_bytes())?;
    Ok(DeribitMessage::PerpetualInterest(
        DeribitPerpetualInterest {
            instrument_name: instrument,
            timestamp: millis_to_nanos(data.timestamp)?,
            interest: data.interest.value(),
            index_price: data.index_price.positive_price()?,
        },
    ))
}

fn parse_instruments_response(raw: &[u8]) -> Result<DeribitMessage, NativeParseError> {
    let envelope: WireInstrumentResponse = strict_decode(raw)?;
    if envelope.jsonrpc != "2.0" || envelope.result.len() > MAX_INSTRUMENTS_PER_RESPONSE {
        return Err(NativeParseError::InvalidValue);
    }
    let mut names = BTreeSet::new();
    let mut ids = BTreeSet::new();
    let mut instruments = Vec::with_capacity(envelope.result.len());
    for wire in envelope.result {
        validate_identifier(&wire.instrument_name)?;
        validate_identifier(&wire.base_currency)?;
        validate_identifier(&wire.quote_currency)?;
        validate_identifier(&wire.settlement_currency)?;
        if wire.instrument_id == 0
            || !names.insert(wire.instrument_name.clone())
            || !ids.insert(wire.instrument_id)
        {
            return Err(NativeParseError::InvalidValue);
        }
        let kind = match wire.kind.as_str() {
            "future" => DeribitInstrumentKind::Future,
            "option" => DeribitInstrumentKind::Option,
            _ => return Err(NativeParseError::InvalidValue),
        };
        let instrument_type = match wire.instrument_type.as_str() {
            "linear" => DeribitInstrumentType::Linear,
            "reversed" => DeribitInstrumentType::Reversed,
            _ => return Err(NativeParseError::InvalidValue),
        };
        if wire
            .future_type
            .as_deref()
            .is_some_and(|value| value != wire.instrument_type)
        {
            return Err(NativeParseError::InvalidValue);
        }
        validate_identifier(&wire.price_index)?;
        validate_identifier(&wire.underlying_type)?;
        for identifier in [
            wire._base_currency_uuid.as_deref(),
            wire._quote_currency_uuid.as_deref(),
            wire._product_group.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            validate_identifier(identifier)?;
        }
        if !matches!(
            wire.settlement_period.as_str(),
            "month" | "week" | "perpetual"
        ) || !wire.contract_size.value().is_positive()
            || wire
                ._max_leverage
                .as_ref()
                .is_some_and(|value| !value.value().is_positive())
            || wire
                ._max_liquidation_commission
                .as_ref()
                .is_some_and(|value| value.value().is_negative())
            || wire
                ._block_trade_min_trade_amount
                .as_ref()
                .is_some_and(|value| !value.value().is_positive())
            || wire
                ._block_trade_tick_size
                .as_ref()
                .is_some_and(|value| !value.value().is_positive())
        {
            return Err(NativeParseError::InvalidValue);
        }
        let mut previous_step = None;
        for step in &wire._tick_size_steps {
            if !step._above_price.value().is_positive()
                || !step._tick_size.value().is_positive()
                || previous_step.is_some_and(|previous| step._above_price.value() <= previous)
            {
                return Err(NativeParseError::InvalidValue);
            }
            previous_step = Some(step._above_price.value());
        }
        let option_kind = match wire.option_type.as_deref() {
            None => None,
            Some("call") => Some(OptionKind::Call),
            Some("put") => Some(OptionKind::Put),
            Some(_) => return Err(NativeParseError::InvalidValue),
        };
        let strike = optional_positive_price(wire.strike.as_ref())?;
        if (kind == DeribitInstrumentKind::Option) != (option_kind.is_some() && strike.is_some()) {
            return Err(NativeParseError::InvalidValue);
        }
        if kind == DeribitInstrumentKind::Option
            && !option_symbol_matches(
                &wire.instrument_name,
                strike.ok_or(NativeParseError::InvalidValue)?,
                option_kind.ok_or(NativeParseError::InvalidValue)?,
            )
        {
            return Err(NativeParseError::InvalidValue);
        }
        if kind == DeribitInstrumentKind::Future
            && (wire.settlement_period == "perpetual")
                != wire.instrument_name.ends_with("-PERPETUAL")
        {
            return Err(NativeParseError::InvalidValue);
        }
        let creation_time = millis_to_nanos(wire.creation_timestamp)?;
        let expiration_time = if wire.settlement_period == "perpetual" {
            None
        } else {
            Some(millis_to_nanos(wire.expiration_timestamp)?)
        };
        if expiration_time.is_some_and(|expiry| expiry.value() <= creation_time.value()) {
            return Err(NativeParseError::InvalidValue);
        }
        instruments.push(DeribitInstrument {
            instrument_name: wire.instrument_name,
            instrument_id: wire.instrument_id,
            kind,
            instrument_type,
            settlement_period: wire.settlement_period,
            price_index: wire.price_index,
            underlying_type: wire.underlying_type,
            active: wire.is_active,
            lifecycle: parse_lifecycle(&wire.state)?,
            base_currency: wire.base_currency,
            quote_currency: wire.quote_currency,
            counter_currency: wire.counter_currency,
            settlement_currency: wire.settlement_currency,
            contract_size: wire.contract_size.value(),
            price_tick: wire.tick_size.positive_price()?,
            quantity_step: wire.qty_tick_size.positive_quantity()?,
            minimum_trade_amount: wire.min_trade_amount.positive_quantity()?,
            creation_time,
            expiration_time,
            strike,
            option_kind,
        });
    }
    Ok(DeribitMessage::Instruments(instruments))
}

fn parse_delivery_response(raw: &[u8]) -> Result<DeribitMessage, NativeParseError> {
    let envelope: WireDeliveryResponse = strict_decode(raw)?;
    if envelope.jsonrpc != "2.0"
        || envelope.result.data.len() > MAX_DELIVERY_RECORDS
        || envelope.result.records_total
            < u64::try_from(envelope.result.data.len()).map_err(|_| NativeParseError::Capacity)?
    {
        return Err(NativeParseError::InvalidValue);
    }
    let mut dates = BTreeSet::new();
    let mut records = Vec::with_capacity(envelope.result.data.len());
    for value in envelope.result.data {
        if !valid_date(&value.date) || !dates.insert(value.date.clone()) {
            return Err(NativeParseError::InvalidValue);
        }
        records.push(DeribitDeliveryPrice {
            date: value.date,
            delivery_price: value.delivery_price.positive_price()?,
        });
    }
    Ok(DeribitMessage::DeliveryPrices {
        records,
        records_total: envelope.result.records_total,
    })
}

fn validate_envelope(envelope: &WireSubscriptionEnvelope) -> Result<(), NativeParseError> {
    if envelope.jsonrpc != "2.0" || envelope.method != "subscription" {
        Err(NativeParseError::UnsupportedMessage)
    } else {
        Ok(())
    }
}

fn validate_channel(
    channel: &str,
    expected_prefix: &str,
    has_interval: bool,
) -> Result<String, NativeParseError> {
    validate_identifier(channel)?;
    let parts = channel.split('.').collect::<Vec<_>>();
    let expected_len = if has_interval { 3 } else { 2 };
    if parts.len() != expected_len || parts[0] != expected_prefix {
        return Err(NativeParseError::UnsupportedMessage);
    }
    if has_interval && !matches!(parts[2], "raw" | "100ms" | "agg2") {
        return Err(NativeParseError::UnsupportedMessage);
    }
    validate_identifier(parts[1])?;
    Ok(parts[1].to_owned())
}

fn parse_levels(
    levels: Vec<(String, ExactDecimal, ExactDecimal)>,
) -> Result<Vec<NativeBookLevel>, NativeParseError> {
    let mut result = Vec::with_capacity(levels.len());
    let mut prices = BTreeSet::new();
    for (action, price, quantity) in levels {
        let action = match action.as_str() {
            "new" => BookAction::New,
            "change" => BookAction::Change,
            "delete" => BookAction::Delete,
            _ => return Err(NativeParseError::InvalidValue),
        };
        let price_value = price.positive_price()?;
        let quantity_value = quantity.quantity()?;
        if !prices.insert(price_value)
            || match action {
                BookAction::New | BookAction::Change => !quantity_value.value().is_positive(),
                BookAction::Delete => !quantity_value.value().is_zero(),
            }
        {
            return Err(NativeParseError::InvalidValue);
        }
        result.push(NativeBookLevel {
            action,
            price: price_value,
            quantity: quantity_value,
            price_text: price.text,
            quantity_text: quantity.text,
        });
    }
    Ok(result)
}

fn validate_ticker_product_fields(ticker: &DeribitTicker) -> Result<(), NativeParseError> {
    let option_fields = ticker.bid_iv.is_some()
        || ticker.ask_iv.is_some()
        || ticker.mark_iv.is_some()
        || ticker.greeks.is_some()
        || ticker.underlying_price.is_some()
        || ticker.underlying_index.is_some()
        || ticker.interest_rate.is_some();
    if option_fields
        && (ticker.mark_iv.is_none()
            || ticker.greeks.is_none()
            || ticker.underlying_price.is_none()
            || ticker.underlying_index.is_none())
    {
        return Err(NativeParseError::InvalidValue);
    }
    if ticker.current_funding.is_some() != ticker.funding_8h.is_some() {
        return Err(NativeParseError::InvalidValue);
    }
    if ticker.minimum_price >= ticker.maximum_price
        || ticker
            .stats
            .low
            .zip(ticker.stats.high)
            .is_some_and(|(low, high)| low > high)
    {
        return Err(NativeParseError::InvalidValue);
    }
    Ok(())
}

fn parse_lifecycle(value: &str) -> Result<DeribitLifecycle, NativeParseError> {
    match value {
        "open" => Ok(DeribitLifecycle::Open),
        "settlement" => Ok(DeribitLifecycle::Settlement),
        "delivered" => Ok(DeribitLifecycle::Delivered),
        "inactive" => Ok(DeribitLifecycle::Inactive),
        "locked" => Ok(DeribitLifecycle::Locked),
        "halted" => Ok(DeribitLifecycle::Halted),
        "archivized" => Ok(DeribitLifecycle::Archivized),
        _ => Err(NativeParseError::InvalidValue),
    }
}

fn option_symbol_matches(name: &str, strike: Price, option_kind: OptionKind) -> bool {
    let expected_side = match option_kind {
        OptionKind::Call => "C",
        OptionKind::Put => "P",
    };
    let mut components = name.rsplitn(3, '-');
    let side = components.next();
    let source_strike = components.next();
    let prefix = components.next();
    let canonical_strike = strike.value().to_string();
    side == Some(expected_side)
        && prefix.is_some_and(|value| !value.is_empty())
        && source_strike.is_some_and(|value| {
            value == canonical_strike || value == canonical_strike.replace('.', "d")
        })
}

fn parse_side(value: &str) -> Result<Side, NativeParseError> {
    match value {
        "buy" => Ok(Side::Buy),
        "sell" => Ok(Side::Sell),
        _ => Err(NativeParseError::InvalidValue),
    }
}

fn optional_positive_price(
    value: Option<&ExactDecimal>,
) -> Result<Option<Price>, NativeParseError> {
    value.map(ExactDecimal::positive_price).transpose()
}

fn optional_nonnegative_quantity(
    value: Option<&ExactDecimal>,
) -> Result<Option<Quantity>, NativeParseError> {
    value
        .map(|decimal| {
            let quantity = decimal.quantity()?;
            if quantity.value().is_negative() {
                Err(NativeParseError::InvalidValue)
            } else {
                Ok(quantity)
            }
        })
        .transpose()
}

fn nonnegative_quantity(value: &ExactDecimal) -> Result<Quantity, NativeParseError> {
    let quantity = value.quantity()?;
    if quantity.value().is_negative() {
        Err(NativeParseError::InvalidValue)
    } else {
        Ok(quantity)
    }
}

fn optional_decimal(value: Option<ExactDecimal>) -> Option<FixedDecimal> {
    value.map(|decimal| decimal.value())
}

fn optional_nonnegative_decimal(
    value: Option<ExactDecimal>,
) -> Result<Option<FixedDecimal>, NativeParseError> {
    value
        .map(|decimal| {
            if decimal.value().is_negative() {
                Err(NativeParseError::InvalidValue)
            } else {
                Ok(decimal.value())
            }
        })
        .transpose()
}

fn parse_scientific_decimal(value: &str) -> Result<FixedDecimal, NativeParseError> {
    let mut exponent_markers = value.match_indices(['e', 'E']);
    let Some((marker, _)) = exponent_markers.next() else {
        return FixedDecimal::parse(value).map_err(|_| NativeParseError::InvalidDecimal);
    };
    if exponent_markers.next().is_some() {
        return Err(NativeParseError::InvalidDecimal);
    }
    let coefficient =
        FixedDecimal::parse(&value[..marker]).map_err(|_| NativeParseError::InvalidDecimal)?;
    let exponent_text = &value[marker + 1..];
    if exponent_text.is_empty()
        || exponent_text == "+"
        || exponent_text == "-"
        || !exponent_text
            .strip_prefix('+')
            .or_else(|| exponent_text.strip_prefix('-'))
            .unwrap_or(exponent_text)
            .bytes()
            .all(|byte| byte.is_ascii_digit())
    {
        return Err(NativeParseError::InvalidDecimal);
    }
    let exponent = exponent_text
        .parse::<i32>()
        .map_err(|_| NativeParseError::InvalidDecimal)?;
    if coefficient.is_zero() {
        return FixedDecimal::new(0, 0).map_err(|_| NativeParseError::InvalidDecimal);
    }
    let adjusted_scale = i64::from(coefficient.scale()) - i64::from(exponent);
    if adjusted_scale >= 0 {
        let scale = u32::try_from(adjusted_scale).map_err(|_| NativeParseError::InvalidDecimal)?;
        return FixedDecimal::new(coefficient.mantissa(), scale)
            .map_err(|_| NativeParseError::InvalidDecimal);
    }
    let shift = u32::try_from(-adjusted_scale).map_err(|_| NativeParseError::InvalidDecimal)?;
    if shift > MAX_SCALE {
        return Err(NativeParseError::InvalidDecimal);
    }
    let mantissa = (0..shift).try_fold(coefficient.mantissa(), |current, _| {
        current
            .checked_mul(10)
            .ok_or(NativeParseError::InvalidDecimal)
    })?;
    FixedDecimal::new(mantissa, 0).map_err(|_| NativeParseError::InvalidDecimal)
}

fn millis_to_nanos(value: u64) -> Result<UnixNanos, NativeParseError> {
    let nanos = value
        .checked_mul(1_000_000)
        .and_then(|value| i64::try_from(value).ok())
        .filter(|value| *value > 0)
        .ok_or(NativeParseError::InvalidValue)?;
    Ok(UnixNanos::new(nanos))
}

fn nanos_from_u64(value: u64) -> Result<UnixNanos, NativeParseError> {
    let nanos = i64::try_from(value)
        .ok()
        .filter(|value| *value > 0)
        .ok_or(NativeParseError::InvalidValue)?;
    Ok(UnixNanos::new(nanos))
}

fn validate_identifier(value: &str) -> Result<(), NativeParseError> {
    if value.is_empty()
        || value.len() > MAX_IDENTIFIER_BYTES
        || !value.is_ascii()
        || value.trim() != value
        || value.chars().any(char::is_control)
    {
        Err(NativeParseError::InvalidValue)
    } else {
        Ok(())
    }
}

fn valid_date(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != 10
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || !bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| index == 4 || index == 7 || byte.is_ascii_digit())
    {
        return false;
    }
    let year = u16::from(bytes[0] - b'0') * 1_000
        + u16::from(bytes[1] - b'0') * 100
        + u16::from(bytes[2] - b'0') * 10
        + u16::from(bytes[3] - b'0');
    let month = (bytes[5] - b'0') * 10 + (bytes[6] - b'0');
    let day = (bytes[8] - b'0') * 10 + (bytes[9] - b'0');
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days_in_month = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return false,
    };
    year != 0 && day != 0 && day <= days_in_month
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
struct WireSubscriptionEnvelope {
    jsonrpc: String,
    method: String,
    params: WireSubscriptionParams,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireSubscriptionParams {
    channel: String,
    data: Box<RawValue>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireBook {
    #[serde(rename = "type")]
    message_type: String,
    timestamp: u64,
    instrument_name: String,
    change_id: u64,
    #[serde(default)]
    prev_change_id: Option<u64>,
    bids: Vec<(String, ExactDecimal, ExactDecimal)>,
    asks: Vec<(String, ExactDecimal, ExactDecimal)>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireTrade {
    trade_seq: u64,
    trade_id: String,
    timestamp: u64,
    #[serde(default)]
    starbase_timestamp: Option<u64>,
    tick_direction: u8,
    price: ExactDecimal,
    mark_price: ExactDecimal,
    instrument_name: String,
    index_price: ExactDecimal,
    direction: String,
    amount: ExactDecimal,
    #[serde(default)]
    contracts: Option<ExactDecimal>,
    #[serde(default)]
    iv: Option<ExactDecimal>,
    #[serde(default)]
    liquidation: Option<String>,
    #[serde(default)]
    block_trade_id: Option<String>,
    #[serde(default)]
    block_trade_leg_count: Option<u64>,
    #[serde(default)]
    combo_id: Option<String>,
    #[serde(default)]
    combo_trade_id: Option<String>,
    #[serde(default)]
    starbase_match_id: Option<u64>,
    #[serde(default)]
    block_rfq_id: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireTicker {
    instrument_name: String,
    timestamp: u64,
    state: String,
    index_price: ExactDecimal,
    min_price: ExactDecimal,
    max_price: ExactDecimal,
    mark_price: ExactDecimal,
    estimated_delivery_price: ExactDecimal,
    open_interest: ExactDecimal,
    last_price: RequiredNullable<ExactDecimal>,
    best_bid_price: RequiredNullable<ExactDecimal>,
    best_bid_amount: ExactDecimal,
    best_ask_price: RequiredNullable<ExactDecimal>,
    best_ask_amount: ExactDecimal,
    #[serde(default)]
    settlement_price: Option<ExactDecimal>,
    #[serde(default)]
    delivery_price: Option<ExactDecimal>,
    #[serde(default)]
    current_funding: Option<ExactDecimal>,
    #[serde(default)]
    funding_8h: Option<ExactDecimal>,
    #[serde(default)]
    interest_value: Option<ExactDecimal>,
    #[serde(default)]
    underlying_price: Option<ExactDecimal>,
    #[serde(default)]
    underlying_index: Option<String>,
    #[serde(default)]
    interest_rate: Option<ExactDecimal>,
    #[serde(default)]
    bid_iv: Option<ExactDecimal>,
    #[serde(default)]
    ask_iv: Option<ExactDecimal>,
    #[serde(default)]
    mark_iv: Option<ExactDecimal>,
    #[serde(default)]
    greeks: Option<WireGreeks>,
    stats: WireStats,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireGreeks {
    delta: ExactDecimal,
    gamma: ExactDecimal,
    rho: ExactDecimal,
    theta: ExactDecimal,
    vega: ExactDecimal,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireStats {
    volume: ExactDecimal,
    low: RequiredNullable<ExactDecimal>,
    high: RequiredNullable<ExactDecimal>,
    price_change: RequiredNullable<ExactDecimal>,
    #[serde(default)]
    volume_usd: Option<ExactDecimal>,
}

#[derive(Deserialize)]
#[serde(transparent)]
struct RequiredNullable<T>(Option<T>);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WirePerpetual {
    interest: ExactDecimal,
    timestamp: u64,
    index_price: ExactDecimal,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireInstrumentResponse {
    jsonrpc: String,
    #[serde(rename = "id")]
    _id: u64,
    result: Vec<WireInstrument>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireInstrument {
    tick_size: ExactDecimal,
    #[serde(default)]
    #[serde(rename = "tick_size_steps")]
    _tick_size_steps: Vec<WireTickSizeStep>,
    #[serde(default)]
    #[serde(rename = "taker_commission")]
    _taker_commission: Option<ExactDecimal>,
    settlement_period: String,
    settlement_currency: String,
    quote_currency: String,
    price_index: String,
    min_trade_amount: ExactDecimal,
    #[serde(default)]
    #[serde(rename = "max_liquidation_commission")]
    _max_liquidation_commission: Option<ExactDecimal>,
    #[serde(default)]
    #[serde(rename = "max_leverage")]
    _max_leverage: Option<ExactDecimal>,
    #[serde(default)]
    #[serde(rename = "maker_commission")]
    _maker_commission: Option<ExactDecimal>,
    kind: String,
    is_active: bool,
    instrument_name: String,
    instrument_id: u64,
    instrument_type: String,
    #[serde(default)]
    future_type: Option<String>,
    expiration_timestamp: u64,
    creation_timestamp: u64,
    #[serde(default)]
    counter_currency: Option<String>,
    contract_size: ExactDecimal,
    #[serde(default)]
    #[serde(rename = "block_trade_tick_size")]
    _block_trade_tick_size: Option<ExactDecimal>,
    #[serde(default)]
    #[serde(rename = "block_trade_min_trade_amount")]
    _block_trade_min_trade_amount: Option<ExactDecimal>,
    #[serde(default)]
    #[serde(rename = "block_trade_commission")]
    _block_trade_commission: Option<ExactDecimal>,
    base_currency: String,
    #[serde(default)]
    #[serde(rename = "base_currency_uuid")]
    _base_currency_uuid: Option<String>,
    #[serde(default)]
    #[serde(rename = "quote_currency_uuid")]
    _quote_currency_uuid: Option<String>,
    state: String,
    qty_tick_size: ExactDecimal,
    #[serde(default)]
    #[serde(rename = "index_id")]
    _index_id: Option<u64>,
    #[serde(default)]
    #[serde(rename = "product_group")]
    _product_group: Option<String>,
    #[serde(default)]
    #[serde(rename = "is_csr")]
    _is_csr: Option<bool>,
    underlying_type: String,
    #[serde(default)]
    strike: Option<ExactDecimal>,
    #[serde(default)]
    option_type: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireTickSizeStep {
    #[serde(rename = "above_price")]
    _above_price: ExactDecimal,
    #[serde(rename = "tick_size")]
    _tick_size: ExactDecimal,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireDeliveryResponse {
    jsonrpc: String,
    #[serde(rename = "id")]
    _id: u64,
    result: WireDeliveryResult,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireDeliveryResult {
    data: Vec<WireDeliveryPrice>,
    records_total: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireDeliveryPrice {
    date: String,
    delivery_price: ExactDecimal,
}
