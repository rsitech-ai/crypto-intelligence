use domain::{ProductType, UnixNanos};
use fixed_decimal::{Price, Quantity};
use serde::Deserialize;
use serde_json::Value;
use thiserror::Error;

use crate::parser::{
    BybitMarket, MAX_NATIVE_PAYLOAD_BYTES, NativeParseError, ensure_allowed,
    parse_unique_json_bounded, price, quantity, timestamp, validate_symbol,
};

const MAX_INSTRUMENTS_PER_PAGE: usize = 1_000;
const MAX_ASSET_SYMBOL_BYTES: usize = 32;
const MAX_CURSOR_BYTES: usize = 4_096;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BybitInstrumentLifecycle {
    Active,
    Pending,
    Inactive,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BybitInstrumentMetadata {
    pub market: BybitMarket,
    pub symbol_id: u64,
    pub symbol: String,
    pub product_type: ProductType,
    pub base_asset: String,
    pub quote_asset: String,
    pub settlement_asset: String,
    pub lifecycle: BybitInstrumentLifecycle,
    pub listing_time: Option<UnixNanos>,
    pub delisting_time: Option<UnixNanos>,
    pub price_tick: Price,
    pub quantity_step: Quantity,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstrumentsInfoReport {
    pub market: BybitMarket,
    pub observed_at: UnixNanos,
    pub next_page_cursor: Option<String>,
    pub instruments: Vec<BybitInstrumentMetadata>,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum MetadataError {
    #[error("Bybit instruments-info payload is invalid: {0}")]
    Native(#[from] NativeParseError),
    #[error("Bybit instruments-info response failed")]
    ProviderFailure,
    #[error("Bybit instruments-info category does not match the requested market")]
    WrongCategory,
    #[error("Bybit instrument product type is unsupported")]
    UnsupportedProduct,
    #[error("Bybit instrument lifecycle is unsupported")]
    UnsupportedLifecycle,
    #[error("Bybit instruments-info collection exceeds its bound")]
    TooManyInstruments,
    #[error("Bybit instruments-info pagination cursor is invalid")]
    InvalidCursor,
}

pub fn parse_instruments_info(
    market: BybitMarket,
    raw: &[u8],
) -> Result<InstrumentsInfoReport, MetadataError> {
    let value = parse_unique_json_bounded(raw, MAX_NATIVE_PAYLOAD_BYTES)?;
    let root = value.as_object().ok_or(NativeParseError::MalformedJson)?;
    ensure_allowed(root, &["retCode", "retMsg", "result", "retExtInfo", "time"])?;
    let result = root
        .get("result")
        .and_then(Value::as_object)
        .ok_or(NativeParseError::MalformedJson)?;
    ensure_allowed(result, &["category", "nextPageCursor", "list"])?;
    let expected_category = match market {
        BybitMarket::Spot => "spot",
        BybitMarket::LinearPerpetual => "linear",
    };
    if result.get("category").and_then(Value::as_str) != Some(expected_category) {
        return Err(MetadataError::WrongCategory);
    }
    let list = result
        .get("list")
        .and_then(Value::as_array)
        .ok_or(NativeParseError::MalformedJson)?;
    if list.is_empty() || list.len() > MAX_INSTRUMENTS_PER_PAGE {
        return Err(if list.len() > MAX_INSTRUMENTS_PER_PAGE {
            MetadataError::TooManyInstruments
        } else {
            MetadataError::Native(NativeParseError::InvalidValue)
        });
    }
    for instrument in list {
        let instrument = instrument
            .as_object()
            .ok_or(NativeParseError::MalformedJson)?;
        match market {
            BybitMarket::Spot => ensure_allowed(
                instrument,
                &[
                    "symbolId",
                    "symbol",
                    "baseCoin",
                    "quoteCoin",
                    "innovation",
                    "symbolType",
                    "xstockMultiplier",
                    "status",
                    "marginTrading",
                    "stTag",
                    "lotSizeFilter",
                    "priceFilter",
                    "riskParameters",
                ],
            )?,
            BybitMarket::LinearPerpetual => ensure_allowed(
                instrument,
                &[
                    "symbol",
                    "symbolId",
                    "contractType",
                    "status",
                    "baseCoin",
                    "quoteCoin",
                    "settleCoin",
                    "symbolType",
                    "launchTime",
                    "deliveryTime",
                    "deliveryFeeRate",
                    "priceScale",
                    "leverageFilter",
                    "priceFilter",
                    "lotSizeFilter",
                    "unifiedMarginTrade",
                    "fundingInterval",
                    "copyTrading",
                    "upperFundingRate",
                    "lowerFundingRate",
                    "displayName",
                    "forbidUplWithdrawal",
                    "isPreListing",
                    "preListingInfo",
                    "riskParameters",
                ],
            )?,
        }
        let price_filter = instrument
            .get("priceFilter")
            .and_then(Value::as_object)
            .ok_or(NativeParseError::MalformedJson)?;
        match market {
            BybitMarket::Spot => ensure_allowed(price_filter, &["tickSize"])?,
            BybitMarket::LinearPerpetual => {
                ensure_allowed(price_filter, &["minPrice", "maxPrice", "tickSize"])?
            }
        }
        let lot_filter = instrument
            .get("lotSizeFilter")
            .and_then(Value::as_object)
            .ok_or(NativeParseError::MalformedJson)?;
        match market {
            BybitMarket::Spot => ensure_allowed(
                lot_filter,
                &[
                    "basePrecision",
                    "quotePrecision",
                    "minOrderQty",
                    "maxOrderQty",
                    "minOrderAmt",
                    "maxOrderAmt",
                    "maxLimitOrderQty",
                    "maxMarketOrderQty",
                    "postOnlyMaxLimitOrderSize",
                ],
            )?,
            BybitMarket::LinearPerpetual => ensure_allowed(
                lot_filter,
                &[
                    "minNotionalValue",
                    "maxOrderQty",
                    "maxMktOrderQty",
                    "maxMarketOrderQty",
                    "minOrderQty",
                    "qtyStep",
                    "postOnlyMaxOrderQty",
                ],
            )?,
        }
    }
    let wire: WireResponse =
        serde_json::from_value(value).map_err(|_| NativeParseError::MalformedJson)?;
    if wire.ret_code != 0 || wire.ret_msg != "OK" {
        return Err(MetadataError::ProviderFailure);
    }
    if wire.result.category != expected_category {
        return Err(MetadataError::WrongCategory);
    }
    let next_page_cursor = validate_cursor(wire.result.next_page_cursor)?;
    if market == BybitMarket::Spot && next_page_cursor.is_some() {
        return Err(MetadataError::InvalidCursor);
    }
    let observed_at = timestamp(wire.time)?;
    let instruments = wire
        .result
        .list
        .into_iter()
        .map(|instrument| normalize_instrument(market, instrument))
        .collect::<Result<Vec<_>, MetadataError>>()?;
    Ok(InstrumentsInfoReport {
        market,
        observed_at,
        next_page_cursor,
        instruments,
    })
}

fn normalize_instrument(
    market: BybitMarket,
    instrument: WireInstrument,
) -> Result<BybitInstrumentMetadata, MetadataError> {
    validate_symbol(&instrument.symbol)?;
    validate_asset(&instrument.base_coin)?;
    validate_asset(&instrument.quote_coin)?;
    let lifecycle = lifecycle(market, &instrument.status)?;
    let (product_type, settlement_asset, listing_time, delisting_time, quantity_step) = match market
    {
        BybitMarket::Spot => {
            if instrument.contract_type.is_some()
                || instrument.settle_coin.is_some()
                || instrument.launch_time.is_some()
                || instrument.delivery_time.is_some()
                || instrument.quantity_step().is_some()
                || instrument.base_precision().is_none()
            {
                return Err(MetadataError::UnsupportedProduct);
            }
            (
                ProductType::Spot,
                instrument.quote_coin.clone(),
                None,
                None,
                quantity(
                    instrument
                        .base_precision()
                        .ok_or(NativeParseError::MalformedJson)?,
                )?,
            )
        }
        BybitMarket::LinearPerpetual => {
            if instrument.contract_type.as_deref() != Some("LinearPerpetual")
                || instrument
                    .funding_interval
                    .is_none_or(|interval| interval == 0)
            {
                return Err(MetadataError::UnsupportedProduct);
            }
            let settlement = instrument
                .settle_coin
                .clone()
                .ok_or(NativeParseError::MalformedJson)?;
            validate_asset(&settlement)?;
            let listing_time = parse_string_timestamp(
                instrument
                    .launch_time
                    .as_deref()
                    .ok_or(NativeParseError::MalformedJson)?,
            )?;
            let delisting_time = match instrument.delivery_time.as_deref() {
                Some("0") => None,
                Some(value) => Some(parse_string_timestamp(value)?),
                None => return Err(NativeParseError::MalformedJson.into()),
            };
            (
                ProductType::Perpetual,
                settlement,
                Some(listing_time),
                delisting_time,
                quantity(
                    instrument
                        .quantity_step()
                        .ok_or(NativeParseError::MalformedJson)?,
                )?,
            )
        }
    };
    if quantity_step.value().is_zero() {
        return Err(NativeParseError::InvalidDecimal.into());
    }
    let price_tick = price(instrument.tick_size())?;
    Ok(BybitInstrumentMetadata {
        market,
        symbol_id: instrument.symbol_id,
        symbol: instrument.symbol,
        product_type,
        base_asset: instrument.base_coin,
        quote_asset: instrument.quote_coin,
        settlement_asset,
        lifecycle,
        listing_time,
        delisting_time,
        price_tick,
        quantity_step,
    })
}

fn lifecycle(market: BybitMarket, status: &str) -> Result<BybitInstrumentLifecycle, MetadataError> {
    match (market, status) {
        (BybitMarket::Spot, "Trading") | (BybitMarket::LinearPerpetual, "Trading") => {
            Ok(BybitInstrumentLifecycle::Active)
        }
        (BybitMarket::LinearPerpetual, "PreLaunch") => Ok(BybitInstrumentLifecycle::Pending),
        (BybitMarket::LinearPerpetual, "Settling" | "Delivering" | "Closed") => {
            Ok(BybitInstrumentLifecycle::Inactive)
        }
        _ => Err(MetadataError::UnsupportedLifecycle),
    }
}

fn validate_asset(asset: &str) -> Result<(), NativeParseError> {
    if asset.is_empty()
        || asset.len() > MAX_ASSET_SYMBOL_BYTES
        || !asset
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
    {
        Err(NativeParseError::InvalidSymbol)
    } else {
        Ok(())
    }
}

fn validate_cursor(cursor: Option<String>) -> Result<Option<String>, MetadataError> {
    match cursor {
        None => Ok(None),
        Some(cursor) if cursor.is_empty() => Ok(None),
        Some(cursor)
            if cursor.len() <= MAX_CURSOR_BYTES
                && cursor.trim() == cursor
                && !cursor.chars().any(char::is_control) =>
        {
            Ok(Some(cursor))
        }
        Some(_) => Err(MetadataError::InvalidCursor),
    }
}

fn parse_string_timestamp(value: &str) -> Result<UnixNanos, NativeParseError> {
    value
        .parse::<u64>()
        .map_err(|_| NativeParseError::InvalidTimestamp)
        .and_then(timestamp)
}

#[derive(Deserialize)]
struct WireResponse {
    #[serde(rename = "retCode")]
    ret_code: i64,
    #[serde(rename = "retMsg")]
    ret_msg: String,
    result: WireResult,
    #[serde(rename = "retExtInfo")]
    _ret_ext_info: Value,
    time: u64,
}

#[derive(Deserialize)]
struct WireResult {
    category: String,
    #[serde(rename = "nextPageCursor")]
    next_page_cursor: Option<String>,
    list: Vec<WireInstrument>,
}

#[derive(Deserialize)]
struct WireInstrument {
    #[serde(rename = "symbolId")]
    symbol_id: u64,
    symbol: String,
    #[serde(rename = "contractType")]
    contract_type: Option<String>,
    status: String,
    #[serde(rename = "baseCoin")]
    base_coin: String,
    #[serde(rename = "quoteCoin")]
    quote_coin: String,
    #[serde(rename = "settleCoin")]
    settle_coin: Option<String>,
    #[serde(rename = "launchTime")]
    launch_time: Option<String>,
    #[serde(rename = "deliveryTime")]
    delivery_time: Option<String>,
    #[serde(rename = "fundingInterval")]
    funding_interval: Option<u64>,
    #[serde(rename = "priceFilter")]
    price_filter: WirePriceFilter,
    #[serde(rename = "lotSizeFilter")]
    lot_size_filter: WireLotSizeFilter,
}

impl WireInstrument {
    fn tick_size(&self) -> &str {
        &self.price_filter.tick_size
    }

    fn base_precision(&self) -> Option<&str> {
        self.lot_size_filter.base_precision.as_deref()
    }

    fn quantity_step(&self) -> Option<&str> {
        self.lot_size_filter.quantity_step.as_deref()
    }
}

#[derive(Deserialize)]
struct WirePriceFilter {
    #[serde(rename = "tickSize")]
    tick_size: String,
}

#[derive(Deserialize)]
struct WireLotSizeFilter {
    #[serde(rename = "basePrecision")]
    base_precision: Option<String>,
    #[serde(rename = "qtyStep")]
    quantity_step: Option<String>,
}
