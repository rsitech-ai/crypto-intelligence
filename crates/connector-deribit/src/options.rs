//! Canonical instrument identity and source-reported Deribit option metrics.

use domain::{
    AssetId, ContractKind, ContractValueUnit, InstrumentDefinition, InstrumentDefinitionInput,
    InstrumentId, OptionSide, ProductType, VenueId,
};
use fixed_decimal::{FixedDecimal, Price, Quantity};
use thiserror::Error;

use crate::parser::{
    DeribitInstrument, DeribitInstrumentKind, DeribitInstrumentType, DeribitTicker, OptionKind,
};

const DERIBIT_VENUE: &str = "deribit";

/// Explicit registry binding for source symbols to canonical assets.
///
/// Deribit inverse products settle in the base coin while their economic
/// quote/counter currency remains distinct. The binding therefore validates
/// canonical assets against complete instrument metadata instead of guessing
/// from the instrument name.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeribitInstrumentBinding {
    instrument_generation: u32,
    base_asset: AssetId,
    quote_asset: AssetId,
    settlement_asset: AssetId,
}

impl DeribitInstrumentBinding {
    pub fn try_new(
        instrument_generation: u32,
        base_asset: AssetId,
        quote_asset: AssetId,
        settlement_asset: AssetId,
    ) -> Result<Self, InstrumentNormalizationError> {
        if instrument_generation == 0 {
            return Err(InstrumentNormalizationError::InvalidBinding);
        }
        Ok(Self {
            instrument_generation,
            base_asset,
            quote_asset,
            settlement_asset,
        })
    }
}

impl DeribitInstrument {
    pub fn product_type(&self) -> ProductType {
        match (self.kind, self.is_perpetual()) {
            (DeribitInstrumentKind::Option, _) => ProductType::Option,
            (DeribitInstrumentKind::Future, true) => ProductType::Perpetual,
            (DeribitInstrumentKind::Future, false) => ProductType::Future,
        }
    }

    pub fn to_definition(
        &self,
        binding: &DeribitInstrumentBinding,
    ) -> Result<InstrumentDefinition, InstrumentNormalizationError> {
        let product_type = self.product_type();
        let (contract_kind, contract_value_unit, expected_quote) = match self.instrument_type {
            DeribitInstrumentType::Linear => (
                ContractKind::Linear,
                ContractValueUnit::Base,
                self.quote_currency.as_str(),
            ),
            DeribitInstrumentType::Reversed => (
                ContractKind::Inverse,
                ContractValueUnit::Quote,
                self.counter_currency
                    .as_deref()
                    .ok_or(InstrumentNormalizationError::MissingCounterCurrency)?,
            ),
        };
        if binding.base_asset.canonical_symbol() != self.base_currency
            || binding.quote_asset.canonical_symbol() != expected_quote
            || binding.settlement_asset.canonical_symbol() != self.settlement_currency
            || self.quote_currency != expected_quote
        {
            return Err(InstrumentNormalizationError::BindingMismatch);
        }
        let (expiry_time, strike, option_side, delisting_time) = match product_type {
            ProductType::Perpetual => (None, None, None, None),
            ProductType::Future => (
                Some(
                    self.expiration_time
                        .ok_or(InstrumentNormalizationError::MissingExpiration)?,
                ),
                None,
                None,
                self.expiration_time,
            ),
            ProductType::Option => (
                Some(
                    self.expiration_time
                        .ok_or(InstrumentNormalizationError::MissingExpiration)?,
                ),
                self.strike,
                self.option_kind.map(|kind| match kind {
                    OptionKind::Call => OptionSide::Call,
                    OptionKind::Put => OptionSide::Put,
                }),
                self.expiration_time,
            ),
            ProductType::Spot => return Err(InstrumentNormalizationError::UnsupportedProduct),
        };
        let venue = VenueId::new(DERIBIT_VENUE)
            .map_err(|_| InstrumentNormalizationError::InvalidBinding)?;
        let id = InstrumentId::new_for_product(
            venue,
            &self.instrument_name,
            product_type,
            binding.instrument_generation,
        )
        .map_err(|_| InstrumentNormalizationError::InvalidBinding)?;
        InstrumentDefinition::new(InstrumentDefinitionInput {
            id,
            product_type,
            base_asset: binding.base_asset.clone(),
            quote_asset: binding.quote_asset.clone(),
            settlement_asset: binding.settlement_asset.clone(),
            contract_multiplier: self.contract_size,
            contract_value_unit,
            contract_kind,
            expiry_time,
            strike,
            option_side,
            price_tick: self.price_tick,
            quantity_step: self.quantity_step,
            listing_time: self.creation_time,
            delisting_time,
        })
        .map_err(|_| InstrumentNormalizationError::InvalidDefinition)
    }
}

/// Deribit-computed option risk values, kept distinct from local surfaces.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceOptionGreeks {
    pub delta: FixedDecimal,
    pub gamma: FixedDecimal,
    pub rho: FixedDecimal,
    pub theta: FixedDecimal,
    pub vega: FixedDecimal,
}

/// Venue observation with a generation-aware canonical option identity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NormalizedOptionTicker {
    pub instrument: InstrumentId,
    pub underlying_price: Price,
    pub underlying_index: String,
    pub mark_price: Price,
    pub index_price: Price,
    pub mark_iv: FixedDecimal,
    pub bid_iv: Option<FixedDecimal>,
    pub ask_iv: Option<FixedDecimal>,
    pub interest_rate: Option<FixedDecimal>,
    pub greeks: SourceOptionGreeks,
    pub open_interest_contracts: Quantity,
}

pub fn normalize_option_ticker(
    metadata: &DeribitInstrument,
    ticker: &DeribitTicker,
    binding: &DeribitInstrumentBinding,
) -> Result<NormalizedOptionTicker, InstrumentNormalizationError> {
    if metadata.kind != DeribitInstrumentKind::Option
        || metadata.instrument_name != ticker.instrument_name
    {
        return Err(InstrumentNormalizationError::TickerMismatch);
    }
    let definition = metadata.to_definition(binding)?;
    let greeks = ticker
        .greeks
        .as_ref()
        .ok_or(InstrumentNormalizationError::MissingOptionMetric)?;
    Ok(NormalizedOptionTicker {
        instrument: definition.id().clone(),
        underlying_price: ticker
            .underlying_price
            .ok_or(InstrumentNormalizationError::MissingOptionMetric)?,
        underlying_index: ticker
            .underlying_index
            .clone()
            .ok_or(InstrumentNormalizationError::MissingOptionMetric)?,
        mark_price: ticker.mark_price,
        index_price: ticker.index_price,
        mark_iv: ticker
            .mark_iv
            .ok_or(InstrumentNormalizationError::MissingOptionMetric)?,
        bid_iv: ticker.bid_iv,
        ask_iv: ticker.ask_iv,
        interest_rate: ticker.interest_rate,
        greeks: SourceOptionGreeks {
            delta: greeks.delta,
            gamma: greeks.gamma,
            rho: greeks.rho,
            theta: greeks.theta,
            vega: greeks.vega,
        },
        open_interest_contracts: ticker.open_interest,
    })
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum InstrumentNormalizationError {
    #[error("instrument registry binding is invalid")]
    InvalidBinding,
    #[error("instrument registry binding does not match source metadata")]
    BindingMismatch,
    #[error("inverse Deribit metadata omits its counter currency")]
    MissingCounterCurrency,
    #[error("expiring Deribit metadata omits its expiration")]
    MissingExpiration,
    #[error("source metadata cannot form a valid canonical definition")]
    InvalidDefinition,
    #[error("source product is unsupported")]
    UnsupportedProduct,
    #[error("option ticker does not match its instrument definition")]
    TickerMismatch,
    #[error("option ticker omits a required venue-reported metric")]
    MissingOptionMetric,
}
