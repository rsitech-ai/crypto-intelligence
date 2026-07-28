use crate::{AssetId, DomainError, UnixNanos, VenueId, ensure_generation, id::normalize_upper};
use fixed_decimal::{FixedDecimal, Notional, Price, Quantity};
use serde::{Deserialize, Deserializer, Serialize, de::Error as _, ser::SerializeStruct as _};
use std::fmt;

const MAX_SYMBOL_LENGTH: usize = 96;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct InstrumentId {
    venue: VenueId,
    venue_symbol: String,
    product_type: ProductType,
    generation: u32,
}

impl Serialize for InstrumentId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let includes_product = self.product_type != ProductType::Spot;
        let mut state =
            serializer.serialize_struct("InstrumentId", if includes_product { 4 } else { 3 })?;
        state.serialize_field("venue", &self.venue)?;
        state.serialize_field("venue_symbol", &self.venue_symbol)?;
        if includes_product {
            state.serialize_field("product_type", &self.product_type)?;
        }
        state.serialize_field("generation", &self.generation)?;
        state.end()
    }
}

impl InstrumentId {
    /// Constructs the backward-compatible Spot identity form.
    pub fn new(
        venue: VenueId,
        venue_symbol: impl AsRef<str>,
        generation: u32,
    ) -> Result<Self, DomainError> {
        Self::new_for_product(venue, venue_symbol, ProductType::Spot, generation)
    }

    pub fn new_for_product(
        venue: VenueId,
        venue_symbol: impl AsRef<str>,
        product_type: ProductType,
        generation: u32,
    ) -> Result<Self, DomainError> {
        ensure_generation(generation)?;
        Ok(Self {
            venue,
            venue_symbol: normalize_upper(
                venue_symbol.as_ref(),
                "venue symbol",
                MAX_SYMBOL_LENGTH,
            )?,
            product_type,
            generation,
        })
    }

    pub fn venue(&self) -> &VenueId {
        &self.venue
    }

    pub fn venue_symbol(&self) -> &str {
        &self.venue_symbol
    }

    pub const fn product_type(&self) -> ProductType {
        self.product_type
    }

    pub const fn generation(&self) -> u32 {
        self.generation
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InstrumentIdWire {
    venue: VenueId,
    venue_symbol: String,
    #[serde(default)]
    product_type: Option<ProductType>,
    generation: u32,
}

impl<'de> Deserialize<'de> for InstrumentId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = InstrumentIdWire::deserialize(deserializer)?;
        Self::new_for_product(
            wire.venue,
            wire.venue_symbol,
            wire.product_type.unwrap_or(ProductType::Spot),
            wire.generation,
        )
        .map_err(D::Error::custom)
    }
}

impl fmt::Display for InstrumentId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.product_type == ProductType::Spot {
            write!(
                formatter,
                "{}:{}:{}",
                self.venue, self.venue_symbol, self.generation
            )
        } else {
            write!(
                formatter,
                "{}:{}:{}:{}",
                self.venue,
                self.product_type.as_str(),
                self.venue_symbol,
                self.generation
            )
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum ProductType {
    Spot = 0,
    Perpetual = 1,
    Future = 2,
    Option = 3,
}

impl ProductType {
    pub const ALL: [Self; 4] = [Self::Spot, Self::Perpetual, Self::Future, Self::Option];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Spot => "spot",
            Self::Perpetual => "perpetual",
            Self::Future => "future",
            Self::Option => "option",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum ContractKind {
    None = 0,
    Linear = 1,
    Inverse = 2,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum ContractValueUnit {
    Base = 0,
    Quote = 1,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum OptionSide {
    Call = 0,
    Put = 1,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InstrumentDefinitionInput {
    pub id: InstrumentId,
    pub product_type: ProductType,
    pub base_asset: AssetId,
    pub quote_asset: AssetId,
    pub settlement_asset: AssetId,
    pub contract_multiplier: FixedDecimal,
    pub contract_value_unit: ContractValueUnit,
    pub contract_kind: ContractKind,
    pub expiry_time: Option<UnixNanos>,
    pub strike: Option<Price>,
    pub option_side: Option<OptionSide>,
    pub price_tick: Price,
    pub quantity_step: Quantity,
    pub listing_time: UnixNanos,
    pub delisting_time: Option<UnixNanos>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InstrumentDefinitionInputWire {
    id: InstrumentIdWire,
    product_type: ProductType,
    base_asset: AssetId,
    quote_asset: AssetId,
    settlement_asset: AssetId,
    contract_multiplier: FixedDecimal,
    contract_value_unit: ContractValueUnit,
    contract_kind: ContractKind,
    expiry_time: Option<UnixNanos>,
    strike: Option<Price>,
    option_side: Option<OptionSide>,
    price_tick: Price,
    quantity_step: Quantity,
    listing_time: UnixNanos,
    delisting_time: Option<UnixNanos>,
}

impl<'de> Deserialize<'de> for InstrumentDefinitionInput {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = InstrumentDefinitionInputWire::deserialize(deserializer)?;
        let id = InstrumentId::new_for_product(
            wire.id.venue,
            wire.id.venue_symbol,
            wire.id.product_type.unwrap_or(wire.product_type),
            wire.id.generation,
        )
        .map_err(D::Error::custom)?;
        Ok(Self {
            id,
            product_type: wire.product_type,
            base_asset: wire.base_asset,
            quote_asset: wire.quote_asset,
            settlement_asset: wire.settlement_asset,
            contract_multiplier: wire.contract_multiplier,
            contract_value_unit: wire.contract_value_unit,
            contract_kind: wire.contract_kind,
            expiry_time: wire.expiry_time,
            strike: wire.strike,
            option_side: wire.option_side,
            price_tick: wire.price_tick,
            quantity_step: wire.quantity_step,
            listing_time: wire.listing_time,
            delisting_time: wire.delisting_time,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct InstrumentDefinition {
    id: InstrumentId,
    product_type: ProductType,
    base_asset: AssetId,
    quote_asset: AssetId,
    settlement_asset: AssetId,
    contract_multiplier: FixedDecimal,
    contract_value_unit: ContractValueUnit,
    contract_kind: ContractKind,
    expiry_time: Option<UnixNanos>,
    strike: Option<Price>,
    option_side: Option<OptionSide>,
    price_tick: Price,
    quantity_step: Quantity,
    listing_time: UnixNanos,
    delisting_time: Option<UnixNanos>,
}

impl InstrumentDefinition {
    pub fn new(input: InstrumentDefinitionInput) -> Result<Self, DomainError> {
        validate_instrument(&input)?;
        Ok(Self {
            id: input.id,
            product_type: input.product_type,
            base_asset: input.base_asset,
            quote_asset: input.quote_asset,
            settlement_asset: input.settlement_asset,
            contract_multiplier: input.contract_multiplier,
            contract_value_unit: input.contract_value_unit,
            contract_kind: input.contract_kind,
            expiry_time: input.expiry_time,
            strike: input.strike,
            option_side: input.option_side,
            price_tick: input.price_tick,
            quantity_step: input.quantity_step,
            listing_time: input.listing_time,
            delisting_time: input.delisting_time,
        })
    }

    pub fn id(&self) -> &InstrumentId {
        &self.id
    }

    pub const fn product_type(&self) -> ProductType {
        self.product_type
    }

    pub fn base_asset(&self) -> &AssetId {
        &self.base_asset
    }

    pub fn quote_asset(&self) -> &AssetId {
        &self.quote_asset
    }

    pub fn settlement_asset(&self) -> &AssetId {
        &self.settlement_asset
    }

    pub const fn contract_multiplier(&self) -> FixedDecimal {
        self.contract_multiplier
    }

    pub const fn contract_value_unit(&self) -> ContractValueUnit {
        self.contract_value_unit
    }

    pub const fn contract_kind(&self) -> ContractKind {
        self.contract_kind
    }

    pub const fn expiry_time(&self) -> Option<UnixNanos> {
        self.expiry_time
    }

    pub const fn strike(&self) -> Option<Price> {
        self.strike
    }

    pub const fn option_side(&self) -> Option<OptionSide> {
        self.option_side
    }

    pub const fn price_tick(&self) -> Price {
        self.price_tick
    }

    pub const fn quantity_step(&self) -> Quantity {
        self.quantity_step
    }

    pub const fn listing_time(&self) -> UnixNanos {
        self.listing_time
    }

    pub const fn delisting_time(&self) -> Option<UnixNanos> {
        self.delisting_time
    }

    /// Returns the exact notional denominated in the instrument's quote asset.
    ///
    /// For spot and linear contracts, the multiplier is base-denominated, so
    /// quote notional includes `price`. For inverse contracts, the multiplier
    /// is already quote-denominated, so quote notional is independent of
    /// `price`. Converting inverse quote notional to base exposure requires
    /// division by price and an explicit rounding policy; that is deliberately
    /// outside this API.
    pub fn quote_notional(
        &self,
        price: Price,
        quantity: Quantity,
    ) -> Result<Notional, DomainError> {
        let value = match self.contract_kind {
            ContractKind::Inverse => quantity
                .value()
                .checked_mul(self.contract_multiplier)
                .map_err(DomainError::Decimal)?,
            ContractKind::None | ContractKind::Linear => price
                .value()
                .checked_product3(quantity.value(), self.contract_multiplier)
                .map_err(DomainError::Decimal)?,
        };
        Notional::new(value).map_err(DomainError::Decimal)
    }
}

impl<'de> Deserialize<'de> for InstrumentDefinition {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(InstrumentDefinitionInput::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

fn validate_instrument(input: &InstrumentDefinitionInput) -> Result<(), DomainError> {
    if input.id.product_type() != input.product_type {
        return Err(DomainError::InvalidInstrument {
            field: "instrument identity product type",
        });
    }
    if input.base_asset == input.quote_asset {
        return Err(DomainError::InvalidInstrument {
            field: "base and quote assets",
        });
    }
    if !input.contract_multiplier.is_positive() {
        return Err(DomainError::InvalidInstrument {
            field: "contract multiplier",
        });
    }
    if !input.quantity_step.value().is_positive() {
        return Err(DomainError::InvalidInstrument {
            field: "quantity step",
        });
    }

    match input.contract_kind {
        ContractKind::None => {
            if input.product_type != ProductType::Spot
                || input.contract_value_unit != ContractValueUnit::Base
                || input.settlement_asset != input.quote_asset
            {
                return Err(DomainError::InvalidInstrument {
                    field: "spot contract metadata",
                });
            }
        }
        ContractKind::Linear => {
            if input.product_type == ProductType::Spot
                || input.contract_value_unit != ContractValueUnit::Base
                || input.settlement_asset != input.quote_asset
            {
                return Err(DomainError::InvalidInstrument {
                    field: "linear contract metadata",
                });
            }
        }
        ContractKind::Inverse => {
            if input.product_type == ProductType::Spot
                || input.contract_value_unit != ContractValueUnit::Quote
                || input.settlement_asset != input.base_asset
            {
                return Err(DomainError::InvalidInstrument {
                    field: "inverse contract metadata",
                });
            }
        }
    }

    let has_expiry = input.expiry_time.is_some();
    let has_strike = input.strike.is_some();
    let has_option_side = input.option_side.is_some();
    match input.product_type {
        ProductType::Spot | ProductType::Perpetual
            if has_expiry || has_strike || has_option_side =>
        {
            return Err(DomainError::InvalidInstrument {
                field: "non-expiring product metadata",
            });
        }
        ProductType::Future if !has_expiry || has_strike || has_option_side => {
            return Err(DomainError::InvalidInstrument {
                field: "future metadata",
            });
        }
        ProductType::Option if !has_expiry || !has_strike || !has_option_side => {
            return Err(DomainError::InvalidInstrument {
                field: "option metadata",
            });
        }
        _ => {}
    }

    if input
        .delisting_time
        .is_some_and(|delisting| delisting.value() <= input.listing_time.value())
    {
        return Err(DomainError::InvalidLifecycle {
            field: "listing and delisting time",
        });
    }
    if let Some(expiry) = input.expiry_time
        && (expiry.value() <= input.listing_time.value()
            || input
                .delisting_time
                .is_some_and(|delisting| expiry.value() > delisting.value()))
    {
        return Err(DomainError::InvalidLifecycle {
            field: "listing, expiry, and delisting time",
        });
    }
    Ok(())
}
