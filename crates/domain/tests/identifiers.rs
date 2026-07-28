use domain::{
    AssetId, AssetNamespace, ContractKind, ContractValueUnit, DomainError, InstrumentDefinition,
    InstrumentDefinitionInput, InstrumentId, OptionSide, ProductType, UnixNanos, VenueId,
};
use fixed_decimal::{DecimalError, FixedDecimal, Notional, Price, Quantity};

fn decimal(value: &str) -> FixedDecimal {
    FixedDecimal::parse_canonical(value).expect("fixture decimal must be canonical")
}

fn price(value: &str) -> Price {
    Price::new(decimal(value)).expect("fixture price must be positive")
}

fn quantity(value: &str) -> Quantity {
    Quantity::new(decimal(value)).expect("fixture quantity must be nonnegative")
}

fn notional(value: &str) -> Notional {
    Notional::new(decimal(value)).expect("fixture notional must be nonnegative")
}

fn native_asset(chain: &str, symbol: &str, generation: u32) -> AssetId {
    AssetId::new(AssetNamespace::Native, chain, "", symbol, generation)
        .expect("native asset fixture must be valid")
}

fn fiat_asset(symbol: &str, generation: u32) -> AssetId {
    AssetId::new(AssetNamespace::Fiat, "", "", symbol, generation)
        .expect("fiat asset fixture must be valid")
}

fn instrument_input(
    symbol: &str,
    generation: u32,
    product_type: ProductType,
    contract_kind: ContractKind,
    contract_value_unit: ContractValueUnit,
) -> InstrumentDefinitionInput {
    let base_asset = native_asset("bitcoin", "BTC", 1);
    let quote_asset = fiat_asset("USD", 1);
    let settlement_asset = match contract_kind {
        ContractKind::Inverse => base_asset.clone(),
        ContractKind::None | ContractKind::Linear => quote_asset.clone(),
    };
    InstrumentDefinitionInput {
        id: InstrumentId::new(
            VenueId::new("deribit").expect("venue fixture must be valid"),
            symbol,
            generation,
        )
        .expect("instrument fixture must be valid"),
        product_type,
        base_asset,
        quote_asset,
        settlement_asset,
        contract_multiplier: decimal("1"),
        contract_value_unit,
        contract_kind,
        expiry_time: None,
        strike: None,
        option_side: None,
        price_tick: price("0.5"),
        quantity_step: quantity("1"),
        listing_time: UnixNanos::new(100),
        delisting_time: None,
    }
}

#[test]
fn symbols_never_replace_generation_aware_asset_or_instrument_identity() {
    let native = native_asset("bitcoin", "BTC", 1);
    let wrapped = AssetId::new(AssetNamespace::Evm, "1", "0xbtc", "WBTC", 1)
        .expect("wrapped asset fixture must be valid");
    assert_ne!(native, wrapped);
    assert_ne!(
        native,
        native_asset("bitcoin", "BTC", 2),
        "asset generation is identity-significant"
    );

    let first = InstrumentDefinition::new(instrument_input(
        "BTC-PERPETUAL",
        1,
        ProductType::Perpetual,
        ContractKind::Linear,
        ContractValueUnit::Base,
    ))
    .expect("first generation must be valid");
    let second = InstrumentDefinition::new(instrument_input(
        "BTC-PERPETUAL",
        2,
        ProductType::Perpetual,
        ContractKind::Linear,
        ContractValueUnit::Base,
    ))
    .expect("second generation must be valid");
    assert_ne!(first.id(), second.id());
    assert_eq!(first.product_type(), ProductType::Perpetual);
}

#[test]
fn asset_namespaces_reject_ambiguous_chain_and_contract_identity() {
    assert!(AssetId::new(AssetNamespace::Native, "", "", "BTC", 1).is_err());
    assert!(AssetId::new(AssetNamespace::Native, "bitcoin", "wrapped", "BTC", 1).is_err());
    assert!(AssetId::new(AssetNamespace::Evm, "1", "", "WETH", 1).is_err());
    assert!(AssetId::new(AssetNamespace::Solana, "", "mint", "SOL", 1).is_err());
    assert!(AssetId::new(AssetNamespace::Fiat, "swift", "", "USD", 1).is_err());
    assert!(AssetId::new(AssetNamespace::Synthetic, "", "contract", "DXY", 1).is_err());
    assert!(AssetId::new(AssetNamespace::Fiat, "", "", "USD", 0).is_err());
}

#[test]
fn case_sensitive_chain_identity_is_preserved_across_construction_and_serde() {
    let uppercase = AssetId::new(AssetNamespace::Solana, "AbC", "MintX", "TOK", 1)
        .expect("case-sensitive Solana identity must be valid");
    let lowercase = AssetId::new(AssetNamespace::Solana, "abc", "MintX", "TOK", 1)
        .expect("distinct case-sensitive Solana identity must be valid");

    assert_ne!(
        uppercase, lowercase,
        "chain identity must not be silently case-folded"
    );
    assert_eq!(uppercase.chain_id(), "AbC");

    let encoded = serde_json::to_string(&uppercase).expect("asset identity must serialize");
    assert_eq!(
        serde_json::from_str::<AssetId>(&encoded).expect("asset identity must revalidate"),
        uppercase
    );
}

#[test]
fn quote_notional_distinguishes_spot_linear_and_inverse_contracts() {
    let spot = InstrumentDefinition::new(instrument_input(
        "BTCUSD",
        1,
        ProductType::Spot,
        ContractKind::None,
        ContractValueUnit::Base,
    ))
    .expect("spot fixture must be valid");
    assert_eq!(
        spot.quote_notional(price("50000"), quantity("2")),
        Ok(notional("100000"))
    );

    let mut linear_input = instrument_input(
        "BTC-PERPETUAL",
        1,
        ProductType::Perpetual,
        ContractKind::Linear,
        ContractValueUnit::Base,
    );
    linear_input.contract_multiplier = decimal("0.001");
    let linear = InstrumentDefinition::new(linear_input).expect("linear fixture must be valid");
    assert_eq!(
        linear.quote_notional(price("50000"), quantity("10")),
        Ok(notional("500"))
    );

    let mut inverse_input = instrument_input(
        "BTC-PERPETUAL-INVERSE",
        1,
        ProductType::Perpetual,
        ContractKind::Inverse,
        ContractValueUnit::Quote,
    );
    inverse_input.contract_multiplier = decimal("100");
    let inverse = InstrumentDefinition::new(inverse_input).expect("inverse fixture must be valid");
    assert_eq!(
        inverse.quote_notional(price("50000"), quantity("10")),
        Ok(notional("1000"))
    );
    assert_eq!(
        inverse.quote_notional(price("25000"), quantity("10")),
        Ok(notional("1000")),
        "inverse quote notional is price-independent; base exposure is a separate conversion"
    );

    let mut cancellation_input = instrument_input(
        "BTC-CANCELLATION",
        1,
        ProductType::Perpetual,
        ContractKind::Linear,
        ContractValueUnit::Base,
    );
    cancellation_input.contract_multiplier = decimal("2");
    let cancellation =
        InstrumentDefinition::new(cancellation_input).expect("linear fixture must be valid");
    let maximum_quantity =
        Quantity::new(FixedDecimal::new(i128::MAX, 0).expect("maximum decimal must construct"))
            .expect("maximum quantity must be nonnegative");
    assert_eq!(
        cancellation.quote_notional(price("0.5"), maximum_quantity),
        Ok(
            Notional::new(FixedDecimal::new(i128::MAX, 0).expect("maximum decimal must construct"))
                .expect("maximum notional must be nonnegative")
        ),
        "global decimal cancellation must occur before any overflowing intermediate product"
    );
    let maximum_price =
        Price::new(FixedDecimal::new(i128::MAX, 0).expect("maximum decimal must construct"))
            .expect("maximum price must be positive");
    assert_eq!(
        cancellation.quote_notional(maximum_price, quantity("0.5")),
        Ok(
            Notional::new(FixedDecimal::new(i128::MAX, 0).expect("maximum decimal must construct"))
                .expect("maximum notional must be nonnegative")
        ),
        "global cancellation must not depend on which multiplicand carries the scale"
    );

    assert_eq!(
        spot.quote_notional(
            Price::new(FixedDecimal::new(i128::MAX, 0).expect("maximum decimal must construct"))
                .expect("maximum price must be positive"),
            quantity("2"),
        ),
        Err(DomainError::Decimal(DecimalError::ArithmeticOverflow))
    );
    assert_eq!(
        inverse.quote_notional(price("50000"), quantity("0")),
        Ok(notional("0"))
    );
}

#[test]
fn instrument_definition_rejects_inconsistent_economic_metadata() {
    let valid = instrument_input(
        "BTC-PERPETUAL",
        1,
        ProductType::Perpetual,
        ContractKind::Linear,
        ContractValueUnit::Base,
    );

    let mut zero_step = valid.clone();
    zero_step.quantity_step = quantity("0");
    assert!(InstrumentDefinition::new(zero_step).is_err());

    let mut zero_multiplier = valid.clone();
    zero_multiplier.contract_multiplier = decimal("0");
    assert!(InstrumentDefinition::new(zero_multiplier).is_err());

    let mut same_assets = valid.clone();
    same_assets.quote_asset = same_assets.base_asset.clone();
    same_assets.settlement_asset = same_assets.base_asset.clone();
    assert!(InstrumentDefinition::new(same_assets).is_err());

    let mut wrong_settlement = valid.clone();
    wrong_settlement.settlement_asset = wrong_settlement.base_asset.clone();
    assert!(InstrumentDefinition::new(wrong_settlement).is_err());

    let mut wrong_unit = valid.clone();
    wrong_unit.contract_value_unit = ContractValueUnit::Quote;
    assert!(InstrumentDefinition::new(wrong_unit).is_err());

    let mut unexpected_expiry = valid;
    unexpected_expiry.expiry_time = Some(UnixNanos::new(200));
    assert!(InstrumentDefinition::new(unexpected_expiry).is_err());
}

#[test]
fn futures_and_options_require_complete_ordered_lifecycle_metadata() {
    let mut future = instrument_input(
        "BTC-30DEC26",
        1,
        ProductType::Future,
        ContractKind::Linear,
        ContractValueUnit::Base,
    );
    assert!(InstrumentDefinition::new(future.clone()).is_err());
    future.expiry_time = Some(UnixNanos::new(200));
    future.delisting_time = Some(UnixNanos::new(200));
    assert!(InstrumentDefinition::new(future).is_ok());

    let mut option = instrument_input(
        "BTC-30DEC26-50000-C",
        1,
        ProductType::Option,
        ContractKind::Linear,
        ContractValueUnit::Base,
    );
    option.expiry_time = Some(UnixNanos::new(200));
    option.delisting_time = Some(UnixNanos::new(200));
    assert!(InstrumentDefinition::new(option.clone()).is_err());
    option.strike = Some(price("50000"));
    option.option_side = Some(OptionSide::Call);
    let option = InstrumentDefinition::new(option).expect("complete option must be valid");
    assert_eq!(option.option_side(), Some(OptionSide::Call));

    let mut invalid_order = instrument_input(
        "BTC-30DEC26",
        2,
        ProductType::Future,
        ContractKind::Linear,
        ContractValueUnit::Base,
    );
    invalid_order.expiry_time = Some(UnixNanos::new(99));
    invalid_order.delisting_time = Some(UnixNanos::new(100));
    assert!(InstrumentDefinition::new(invalid_order).is_err());
}

#[test]
fn instrument_deserialization_revalidates_private_domain_state() {
    let definition = InstrumentDefinition::new(instrument_input(
        "BTC-PERPETUAL",
        1,
        ProductType::Perpetual,
        ContractKind::Linear,
        ContractValueUnit::Base,
    ))
    .expect("instrument fixture must be valid");
    let encoded = serde_json::to_string(&definition).expect("definition must serialize");
    assert_eq!(
        serde_json::from_str::<InstrumentDefinition>(&encoded)
            .expect("serialized definition must revalidate"),
        definition
    );

    let invalid = encoded.replace("\"quantity_step\":\"1\"", "\"quantity_step\":\"0\"");
    assert_ne!(invalid, encoded, "fixture mutation must be effective");
    assert!(serde_json::from_str::<InstrumentDefinition>(&invalid).is_err());
}
