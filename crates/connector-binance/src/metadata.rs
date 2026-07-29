use domain::{
    AssetId, ContractKind, ContractValueUnit, InstrumentDefinition, InstrumentDefinitionInput,
    InstrumentId, ProductType, UnixNanos, VenueId,
};
use fixed_decimal::{FixedDecimal, Price, Quantity};
use serde_json::{Map, Value};
use thiserror::Error;

use crate::native::parse_unique_json_bounded;
use crate::{BinanceMarket, NativeParseError};

const MAX_EXCHANGE_INFO_BYTES: usize = 8 * 1024 * 1024;
const MAX_SYMBOLS: usize = 10_000;
const MAX_FILTERS_PER_SYMBOL: usize = 128;
const BINANCE_VENUE: &str = "binance";

/// Parsed supported instruments plus explicit accounting for records that are
/// outside this connector's product, identity, or active-lifecycle boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExchangeInfoReport {
    instruments: Vec<BinanceInstrumentMetadata>,
    pending_instruments: Vec<BinanceInstrumentMetadata>,
    lifecycle: Vec<BinanceInstrumentLifecycle>,
    skipped_unsupported_symbols: usize,
    skipped_unsupported_contracts: usize,
    skipped_inactive: usize,
}

impl ExchangeInfoReport {
    pub fn instruments(&self) -> &[BinanceInstrumentMetadata] {
        &self.instruments
    }

    pub fn pending_instruments(&self) -> &[BinanceInstrumentMetadata] {
        &self.pending_instruments
    }

    pub fn lifecycle(&self) -> &[BinanceInstrumentLifecycle] {
        &self.lifecycle
    }

    pub const fn skipped_unsupported_symbols(&self) -> usize {
        self.skipped_unsupported_symbols
    }

    pub const fn skipped_unsupported_contracts(&self) -> usize {
        self.skipped_unsupported_contracts
    }

    pub const fn skipped_inactive(&self) -> usize {
        self.skipped_inactive
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BinanceInstrumentLifecycleKind {
    Inactive,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BinanceInstrumentLifecycle {
    market: BinanceMarket,
    symbol: String,
    status: String,
    kind: BinanceInstrumentLifecycleKind,
    onboard_time: Option<UnixNanos>,
    delivery_time: Option<UnixNanos>,
}

impl BinanceInstrumentLifecycle {
    pub const fn market(&self) -> BinanceMarket {
        self.market
    }

    pub fn symbol(&self) -> &str {
        &self.symbol
    }

    pub fn status(&self) -> &str {
        &self.status
    }

    pub const fn kind(&self) -> BinanceInstrumentLifecycleKind {
        self.kind
    }

    pub const fn onboard_time(&self) -> Option<UnixNanos> {
        self.onboard_time
    }

    pub const fn delivery_time(&self) -> Option<UnixNanos> {
        self.delivery_time
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BinanceInstrumentMetadata {
    market: BinanceMarket,
    symbol: String,
    status: String,
    base_asset_symbol: String,
    quote_asset_symbol: String,
    settlement_asset_symbol: String,
    price_tick: Price,
    quantity_step: Quantity,
    listing_time: Option<UnixNanos>,
    delivery_time: Option<UnixNanos>,
}

impl BinanceInstrumentMetadata {
    pub const fn market(&self) -> BinanceMarket {
        self.market
    }

    pub fn symbol(&self) -> &str {
        &self.symbol
    }

    pub fn status(&self) -> &str {
        &self.status
    }

    pub fn base_asset_symbol(&self) -> &str {
        &self.base_asset_symbol
    }

    pub fn quote_asset_symbol(&self) -> &str {
        &self.quote_asset_symbol
    }

    pub fn settlement_asset_symbol(&self) -> &str {
        &self.settlement_asset_symbol
    }

    pub const fn price_tick(&self) -> Price {
        self.price_tick
    }

    pub const fn quantity_step(&self) -> Quantity {
        self.quantity_step
    }

    pub const fn listing_time(&self) -> Option<UnixNanos> {
        self.listing_time
    }

    pub const fn delivery_time(&self) -> Option<UnixNanos> {
        self.delivery_time
    }

    pub fn to_definition(
        &self,
        binding: &InstrumentMetadataBinding,
    ) -> Result<InstrumentDefinition, MetadataError> {
        if binding.base_asset.canonical_symbol() != self.base_asset_symbol
            || binding.quote_asset.canonical_symbol() != self.quote_asset_symbol
            || binding.settlement_asset.canonical_symbol() != self.settlement_asset_symbol
        {
            return Err(MetadataError::BindingMismatch);
        }
        let listing_time = match (self.listing_time, binding.fallback_listing_time) {
            (Some(source), None) => source,
            (Some(source), Some(fallback)) if source == fallback => source,
            (Some(_), Some(_)) => return Err(MetadataError::BindingMismatch),
            (None, Some(fallback)) => fallback,
            (None, None) => return Err(MetadataError::MissingListingTime),
        };
        let product_type = match self.market {
            BinanceMarket::Spot => ProductType::Spot,
            BinanceMarket::UsdMarginedPerpetual => ProductType::Perpetual,
        };
        let venue = VenueId::new(BINANCE_VENUE).map_err(|_| MetadataError::InvalidBinding)?;
        let id = InstrumentId::new_for_product(
            venue,
            &self.symbol,
            product_type,
            binding.instrument_generation,
        )
        .map_err(|_| MetadataError::InvalidBinding)?;
        InstrumentDefinition::new(InstrumentDefinitionInput {
            id,
            product_type,
            base_asset: binding.base_asset.clone(),
            quote_asset: binding.quote_asset.clone(),
            settlement_asset: binding.settlement_asset.clone(),
            contract_multiplier: FixedDecimal::new(1, 0)
                .map_err(|_| MetadataError::InvalidBinding)?,
            contract_value_unit: ContractValueUnit::Base,
            contract_kind: match self.market {
                BinanceMarket::Spot => ContractKind::None,
                BinanceMarket::UsdMarginedPerpetual => ContractKind::Linear,
            },
            expiry_time: None,
            strike: None,
            option_side: None,
            price_tick: self.price_tick,
            quantity_step: self.quantity_step,
            listing_time,
            delisting_time: None,
        })
        .map_err(|_| MetadataError::InvalidBinding)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstrumentMetadataBinding {
    instrument_generation: u32,
    base_asset: AssetId,
    quote_asset: AssetId,
    settlement_asset: AssetId,
    fallback_listing_time: Option<UnixNanos>,
}

impl InstrumentMetadataBinding {
    pub fn try_new(
        instrument_generation: u32,
        base_asset: AssetId,
        quote_asset: AssetId,
        settlement_asset: AssetId,
        fallback_listing_time: Option<UnixNanos>,
    ) -> Result<Self, MetadataError> {
        if instrument_generation == 0 {
            return Err(MetadataError::InvalidBinding);
        }
        Ok(Self {
            instrument_generation,
            base_asset,
            quote_asset,
            settlement_asset,
            fallback_listing_time,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum MetadataError {
    #[error("exchange-info payload is empty")]
    EmptyPayload,
    #[error("exchange-info payload exceeds the parser bound")]
    PayloadTooLarge,
    #[error("exchange-info payload is malformed")]
    MalformedJson,
    #[error("exchange-info payload contains undocumented fields")]
    SchemaDrift,
    #[error("exchange-info payload contains too many records")]
    CapacityExceeded,
    #[error("exchange-info symbol is invalid")]
    InvalidSymbol,
    #[error("exchange-info decimal is invalid")]
    InvalidDecimal,
    #[error("exchange-info timestamp is invalid")]
    InvalidTimestamp,
    #[error("exchange-info contract is unsupported")]
    UnsupportedContract,
    #[error("exchange-info omits a required price or lot-size filter")]
    MissingFilter,
    #[error("exchange-info repeats a price or lot-size filter")]
    DuplicateFilter,
    #[error("instrument identity binding is invalid")]
    InvalidBinding,
    #[error("instrument identity binding does not match source metadata")]
    BindingMismatch,
    #[error("instrument listing time is unavailable")]
    MissingListingTime,
}

/// Parses a bounded `exchangeInfo` response.
///
/// Production callers must request symbol-filtered or otherwise bounded
/// batches so the response remains below both this parser's 8 MiB limit and
/// the raw-capture plane's 16 MiB limit.
pub fn parse_exchange_info_report(
    market: BinanceMarket,
    raw: &[u8],
) -> Result<ExchangeInfoReport, MetadataError> {
    let value = parse_unique_json_bounded(raw, MAX_EXCHANGE_INFO_BYTES).map_err(map_parse_error)?;
    let root = value.as_object().ok_or(MetadataError::MalformedJson)?;
    ensure_allowed(
        root,
        match market {
            BinanceMarket::Spot => &[
                "timezone",
                "serverTime",
                "rateLimits",
                "exchangeFilters",
                "symbols",
            ],
            BinanceMarket::UsdMarginedPerpetual => &[
                "exchangeFilters",
                "rateLimits",
                "serverTime",
                "assets",
                "symbols",
                "timezone",
                "futuresType",
            ],
        },
    )?;
    required_string(root, "timezone")?;
    required_u64(root, "serverTime")?;
    required_array(root, "rateLimits")?;
    required_array(root, "exchangeFilters")?;
    if market == BinanceMarket::UsdMarginedPerpetual {
        required_array(root, "assets")?;
    }
    let symbols = required_array(root, "symbols")?;
    if symbols.is_empty() || symbols.len() > MAX_SYMBOLS {
        return Err(MetadataError::CapacityExceeded);
    }
    let mut report = ExchangeInfoReport {
        instruments: Vec::with_capacity(symbols.len()),
        pending_instruments: Vec::new(),
        lifecycle: Vec::new(),
        skipped_unsupported_symbols: 0,
        skipped_unsupported_contracts: 0,
        skipped_inactive: 0,
    };
    for value in symbols {
        match classify_symbol(market, value)? {
            SymbolDisposition::Supported => report.instruments.push(parse_symbol(market, value)?),
            SymbolDisposition::Pending => {
                report
                    .pending_instruments
                    .push(parse_symbol(market, value)?);
            }
            SymbolDisposition::UnsupportedSymbol => {
                report.skipped_unsupported_symbols += 1;
            }
            SymbolDisposition::UnsupportedContract => {
                report.skipped_unsupported_contracts += 1;
            }
            SymbolDisposition::Inactive => {
                report.skipped_inactive += 1;
                report.lifecycle.push(parse_lifecycle(market, value)?);
            }
        }
    }
    Ok(report)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SymbolDisposition {
    Supported,
    Pending,
    UnsupportedSymbol,
    UnsupportedContract,
    Inactive,
}

fn classify_symbol(
    market: BinanceMarket,
    value: &Value,
) -> Result<SymbolDisposition, MetadataError> {
    let object = value.as_object().ok_or(MetadataError::MalformedJson)?;
    ensure_symbol_fields_allowed(market, object)?;
    let symbol = required_string(object, "symbol")?;
    if !is_supported_symbol(symbol) {
        return Ok(SymbolDisposition::UnsupportedSymbol);
    }
    let status = required_token(object, "status")?;
    match market {
        BinanceMarket::Spot => match status.as_str() {
            "TRADING" => Ok(SymbolDisposition::Supported),
            "HALT" | "BREAK" => Ok(SymbolDisposition::Inactive),
            _ => Err(MetadataError::SchemaDrift),
        },
        BinanceMarket::UsdMarginedPerpetual => {
            let contract_type = required_token(object, "contractType")?;
            match (contract_type.as_str(), status.as_str()) {
                ("PERPETUAL", "TRADING") => Ok(SymbolDisposition::Supported),
                ("PERPETUAL", "PENDING_TRADING") => Ok(SymbolDisposition::Pending),
                ("PERPETUAL", "PRE_SETTLE" | "SETTLING" | "CLOSE")
                | ("PERPETUAL_DELIVERING", "PRE_DELIVERING" | "DELIVERING" | "DELIVERED") => {
                    Ok(SymbolDisposition::Inactive)
                }
                ("PERPETUAL" | "PERPETUAL_DELIVERING", _) => Err(MetadataError::SchemaDrift),
                _ => Ok(SymbolDisposition::UnsupportedContract),
            }
        }
    }
}

fn parse_symbol(
    market: BinanceMarket,
    value: &Value,
) -> Result<BinanceInstrumentMetadata, MetadataError> {
    let object = value.as_object().ok_or(MetadataError::MalformedJson)?;
    ensure_symbol_fields_allowed(market, object)?;

    let symbol = required_symbol(object, "symbol")?;
    let status = required_token(object, "status")?;
    let base_asset_symbol = required_symbol(object, "baseAsset")?;
    let quote_asset_symbol = required_symbol(object, "quoteAsset")?;
    let (settlement_asset_symbol, listing_time, delivery_time) = match market {
        BinanceMarket::Spot => (quote_asset_symbol.clone(), None, None),
        BinanceMarket::UsdMarginedPerpetual => {
            if required_symbol(object, "pair")? != symbol
                || required_token(object, "contractType")? != "PERPETUAL"
            {
                return Err(MetadataError::UnsupportedContract);
            }
            let delivery_time = timestamp(required_u64(object, "deliveryDate")?)?;
            let listing_time = timestamp(required_u64(object, "onboardDate")?)?;
            (
                required_symbol(object, "marginAsset")?,
                Some(listing_time),
                Some(delivery_time),
            )
        }
    };
    let filters = required_array(object, "filters")?;
    if filters.len() > MAX_FILTERS_PER_SYMBOL {
        return Err(MetadataError::CapacityExceeded);
    }
    let (price_tick, quantity_step) = parse_filters(filters)?;
    Ok(BinanceInstrumentMetadata {
        market,
        symbol,
        status,
        base_asset_symbol,
        quote_asset_symbol,
        settlement_asset_symbol,
        price_tick,
        quantity_step,
        listing_time,
        delivery_time,
    })
}

fn parse_lifecycle(
    market: BinanceMarket,
    value: &Value,
) -> Result<BinanceInstrumentLifecycle, MetadataError> {
    let object = value.as_object().ok_or(MetadataError::MalformedJson)?;
    let symbol = required_symbol(object, "symbol")?;
    let status = required_token(object, "status")?;
    let (onboard_time, delivery_time) = match market {
        BinanceMarket::Spot => (None, None),
        BinanceMarket::UsdMarginedPerpetual => (
            Some(timestamp(required_u64(object, "onboardDate")?)?),
            Some(timestamp(required_u64(object, "deliveryDate")?)?),
        ),
    };
    Ok(BinanceInstrumentLifecycle {
        market,
        symbol,
        status,
        kind: BinanceInstrumentLifecycleKind::Inactive,
        onboard_time,
        delivery_time,
    })
}

fn ensure_symbol_fields_allowed(
    market: BinanceMarket,
    object: &Map<String, Value>,
) -> Result<(), MetadataError> {
    ensure_allowed(
        object,
        match market {
            BinanceMarket::Spot => &[
                "symbol",
                "status",
                "baseAsset",
                "baseAssetPrecision",
                "quoteAsset",
                "quotePrecision",
                "quoteAssetPrecision",
                "baseCommissionPrecision",
                "quoteCommissionPrecision",
                "orderTypes",
                "icebergAllowed",
                "ocoAllowed",
                "opoAllowed",
                "otoAllowed",
                "pegInstructionsAllowed",
                "quoteOrderQtyMarketAllowed",
                "allowTrailingStop",
                "cancelReplaceAllowed",
                "amendAllowed",
                "isSpotTradingAllowed",
                "isMarginTradingAllowed",
                "filters",
                "permissions",
                "permissionSets",
                "defaultSelfTradePreventionMode",
                "allowedSelfTradePreventionModes",
            ],
            BinanceMarket::UsdMarginedPerpetual => &[
                "symbol",
                "pair",
                "contractType",
                "deliveryDate",
                "onboardDate",
                "status",
                "maintMarginPercent",
                "requiredMarginPercent",
                "baseAsset",
                "quoteAsset",
                "marginAsset",
                "pricePrecision",
                "quantityPrecision",
                "baseAssetPrecision",
                "quotePrecision",
                "underlyingType",
                "underlyingSubType",
                "settlePlan",
                "triggerProtect",
                "filters",
                "orderTypes",
                "timeInForce",
                "liquidationFee",
                "marketTakeBound",
                "maxMoveOrderLimit",
                "permissionSets",
            ],
        },
    )
}

fn parse_filters(filters: &[Value]) -> Result<(Price, Quantity), MetadataError> {
    let mut price_tick = None;
    let mut quantity_step = None;
    for filter in filters {
        let object = filter.as_object().ok_or(MetadataError::MalformedJson)?;
        ensure_allowed(
            object,
            &[
                "filterType",
                "minPrice",
                "maxPrice",
                "tickSize",
                "minQty",
                "maxQty",
                "stepSize",
                "limit",
                "notional",
                "minNotional",
                "maxNotional",
                "applyToMarket",
                "applyMinToMarket",
                "applyMaxToMarket",
                "avgPriceMins",
                "multiplierUp",
                "multiplierDown",
                "multiplierDecimal",
                "bidMultiplierUp",
                "bidMultiplierDown",
                "askMultiplierUp",
                "askMultiplierDown",
                "maxNumOrders",
                "maxNumAlgoOrders",
                "maxNumIcebergOrders",
                "maxPosition",
                "minTrailingAboveDelta",
                "maxTrailingAboveDelta",
                "minTrailingBelowDelta",
                "maxTrailingBelowDelta",
                "maxNumOrderAmends",
                "maxNumOrderLists",
                "positionControlSide",
            ],
        )?;
        match required_token(object, "filterType")?.as_str() {
            "PRICE_FILTER" => {
                if price_tick.is_some() {
                    return Err(MetadataError::DuplicateFilter);
                }
                price_tick = Some(positive_price(required_string(object, "tickSize")?)?);
            }
            "LOT_SIZE" => {
                if quantity_step.is_some() {
                    return Err(MetadataError::DuplicateFilter);
                }
                quantity_step = Some(positive_quantity(required_string(object, "stepSize")?)?);
            }
            _ => {}
        }
    }
    price_tick
        .zip(quantity_step)
        .ok_or(MetadataError::MissingFilter)
}

fn ensure_allowed(object: &Map<String, Value>, allowed: &[&str]) -> Result<(), MetadataError> {
    if object.keys().any(|key| !allowed.contains(&key.as_str())) {
        Err(MetadataError::SchemaDrift)
    } else {
        Ok(())
    }
}

fn required_array<'a>(
    object: &'a Map<String, Value>,
    field: &str,
) -> Result<&'a [Value], MetadataError> {
    object
        .get(field)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .ok_or(MetadataError::MalformedJson)
}

fn required_string<'a>(
    object: &'a Map<String, Value>,
    field: &str,
) -> Result<&'a str, MetadataError> {
    object
        .get(field)
        .and_then(Value::as_str)
        .ok_or(MetadataError::MalformedJson)
}

fn required_u64(object: &Map<String, Value>, field: &str) -> Result<u64, MetadataError> {
    object
        .get(field)
        .and_then(Value::as_u64)
        .ok_or(MetadataError::MalformedJson)
}

fn required_symbol(object: &Map<String, Value>, field: &str) -> Result<String, MetadataError> {
    let value = required_string(object, field)?;
    if !is_supported_symbol(value) {
        Err(MetadataError::InvalidSymbol)
    } else {
        Ok(value.to_owned())
    }
}

fn is_supported_symbol(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 96
        && value
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
}

fn required_token(object: &Map<String, Value>, field: &str) -> Result<String, MetadataError> {
    let value = required_string(object, field)?;
    if value.is_empty()
        || value.len() > 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
    {
        Err(MetadataError::InvalidSymbol)
    } else {
        Ok(value.to_owned())
    }
}

fn positive_price(value: &str) -> Result<Price, MetadataError> {
    Price::new(FixedDecimal::parse(value).map_err(|_| MetadataError::InvalidDecimal)?)
        .map_err(|_| MetadataError::InvalidDecimal)
}

fn positive_quantity(value: &str) -> Result<Quantity, MetadataError> {
    let value =
        Quantity::new(FixedDecimal::parse(value).map_err(|_| MetadataError::InvalidDecimal)?)
            .map_err(|_| MetadataError::InvalidDecimal)?;
    if value.value().is_zero() {
        Err(MetadataError::InvalidDecimal)
    } else {
        Ok(value)
    }
}

fn timestamp(milliseconds: u64) -> Result<UnixNanos, MetadataError> {
    let nanos = i64::try_from(milliseconds)
        .ok()
        .and_then(|value| value.checked_mul(1_000_000))
        .ok_or(MetadataError::InvalidTimestamp)?;
    if milliseconds < 1_000_000_000_000 {
        Err(MetadataError::InvalidTimestamp)
    } else {
        Ok(UnixNanos::new(nanos))
    }
}

fn map_parse_error(error: NativeParseError) -> MetadataError {
    match error {
        NativeParseError::EmptyPayload => MetadataError::EmptyPayload,
        NativeParseError::PayloadTooLarge => MetadataError::PayloadTooLarge,
        NativeParseError::MalformedJson => MetadataError::MalformedJson,
        NativeParseError::SchemaDrift
        | NativeParseError::WrongRoute
        | NativeParseError::UnsupportedEvent
        | NativeParseError::UnsupportedMarketCategory
        | NativeParseError::InvalidSymbol
        | NativeParseError::InvalidDecimal
        | NativeParseError::InvalidTimestamp
        | NativeParseError::InvalidSequence
        | NativeParseError::TooManyLevels
        | NativeParseError::InvalidValue
        | NativeParseError::RawPayloadMismatch => MetadataError::MalformedJson,
    }
}
