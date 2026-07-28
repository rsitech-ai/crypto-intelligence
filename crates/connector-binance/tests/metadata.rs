use connector_binance::{
    BinanceInput, BinanceInstrumentLifecycleKind, BinanceMarket, BinanceMessage,
    InstrumentMetadataBinding, MetadataError, parse_exchange_info_report, parse_native_message,
};
use domain::{AssetId, AssetNamespace, ProductType, UnixNanos};
use event_envelope::VenueState;
use fixed_decimal::FixedDecimal;

const SPOT_EXCHANGE_INFO: &[u8] =
    include_bytes!("../../../fixtures/exchanges/binance/spot-exchange-info.json");
const USDM_EXCHANGE_INFO: &[u8] =
    include_bytes!("../../../fixtures/exchanges/binance/usdm-exchange-info.json");
const SYSTEM_STATUS: &[u8] =
    include_bytes!("../../../fixtures/exchanges/binance/system-status.json");

fn asset(namespace: AssetNamespace, chain: &str, symbol: &str) -> AssetId {
    AssetId::new(namespace, chain, "", symbol, 1).expect("asset")
}

#[test]
fn exchange_info_uses_authoritative_filters_and_requires_explicit_identity_binding() {
    let spot = parse_exchange_info_report(BinanceMarket::Spot, SPOT_EXCHANGE_INFO)
        .expect("spot exchange info");
    assert_eq!(spot.instruments().len(), 1);
    assert_eq!(
        spot.instruments()[0].price_tick().value(),
        FixedDecimal::parse("0.01000000").expect("tick")
    );

    let futures =
        parse_exchange_info_report(BinanceMarket::UsdMarginedPerpetual, USDM_EXCHANGE_INFO)
            .expect("USD-M exchange info");
    assert_eq!(
        futures.instruments()[0].price_tick().value(),
        FixedDecimal::parse("0.10").expect("tick")
    );
    assert_eq!(
        futures.instruments()[0].quantity_step().value(),
        FixedDecimal::parse("0.001").expect("step")
    );
    assert_ne!(
        futures.instruments()[0].price_tick().value(),
        FixedDecimal::parse("0.000000001").expect("misleading precision")
    );

    let binding = InstrumentMetadataBinding::try_new(
        7,
        asset(AssetNamespace::Native, "bitcoin", "BTC"),
        asset(AssetNamespace::Synthetic, "", "USDT"),
        asset(AssetNamespace::Synthetic, "", "USDT"),
        None,
    )
    .expect("binding");
    let definition = futures.instruments()[0]
        .to_definition(&binding)
        .expect("bound definition");
    assert_eq!(definition.product_type(), ProductType::Perpetual);
    assert_eq!(definition.id().generation(), 7);
    assert_eq!(
        definition.listing_time(),
        UnixNanos::new(1_569_398_400_000_000_000)
    );
}

#[test]
fn exchange_info_rejects_schema_drift_duplicate_filters_and_missing_listing_lineage() {
    let mut drift: serde_json::Value =
        serde_json::from_slice(SPOT_EXCHANGE_INFO).expect("fixture JSON");
    drift
        .as_object_mut()
        .expect("root")
        .insert("mystery".to_owned(), serde_json::json!(1));
    assert_eq!(
        parse_exchange_info_report(
            BinanceMarket::Spot,
            &serde_json::to_vec(&drift).expect("drift JSON")
        ),
        Err(MetadataError::SchemaDrift)
    );

    let mut duplicate: serde_json::Value =
        serde_json::from_slice(SPOT_EXCHANGE_INFO).expect("fixture JSON");
    let filters = duplicate["symbols"][0]["filters"]
        .as_array_mut()
        .expect("filters");
    filters.push(filters[0].clone());
    assert_eq!(
        parse_exchange_info_report(
            BinanceMarket::Spot,
            &serde_json::to_vec(&duplicate).expect("duplicate JSON")
        ),
        Err(MetadataError::DuplicateFilter)
    );

    let spot = parse_exchange_info_report(BinanceMarket::Spot, SPOT_EXCHANGE_INFO).expect("spot");
    let binding = InstrumentMetadataBinding::try_new(
        1,
        asset(AssetNamespace::Native, "bitcoin", "BTC"),
        asset(AssetNamespace::Synthetic, "", "USDT"),
        asset(AssetNamespace::Synthetic, "", "USDT"),
        None,
    )
    .expect("binding");
    assert_eq!(
        spot.instruments()[0].to_definition(&binding),
        Err(MetadataError::MissingListingTime)
    );
}

#[test]
fn system_status_is_typed_and_fails_closed_on_unknown_codes() {
    assert_eq!(
        parse_native_message(BinanceInput::SystemStatusRest, SYSTEM_STATUS).expect("normal status"),
        BinanceMessage::VenueStatus {
            state: VenueState::Operational,
            message: "normal".to_owned(),
        }
    );
    assert_eq!(
        parse_native_message(
            BinanceInput::SystemStatusRest,
            br#"{"status":1,"msg":"system maintenance"}"#
        )
        .expect("maintenance status"),
        BinanceMessage::VenueStatus {
            state: VenueState::Maintenance,
            message: "system maintenance".to_owned(),
        }
    );
    assert!(
        parse_native_message(
            BinanceInput::SystemStatusRest,
            br#"{"status":2,"msg":"unknown"}"#
        )
        .is_err()
    );
}

#[test]
fn current_exchange_info_fields_skip_only_explicitly_unsupported_or_inactive_records() {
    let mut spot: serde_json::Value =
        serde_json::from_slice(SPOT_EXCHANGE_INFO).expect("spot fixture");
    let supported = spot["symbols"][0]
        .as_object_mut()
        .expect("supported symbol");
    supported.insert("opoAllowed".to_owned(), serde_json::json!(true));
    supported.insert("pegInstructionsAllowed".to_owned(), serde_json::json!(true));
    let mut unicode = spot["symbols"][0].clone();
    unicode["symbol"] = serde_json::json!("币安人生USDT");
    unicode["baseAsset"] = serde_json::json!("币安人生");
    let mut inactive = spot["symbols"][0].clone();
    inactive["symbol"] = serde_json::json!("ETHUSDT");
    inactive["baseAsset"] = serde_json::json!("ETH");
    inactive["status"] = serde_json::json!("BREAK");
    spot["symbols"]
        .as_array_mut()
        .expect("symbols")
        .extend([unicode, inactive]);

    let report = parse_exchange_info_report(
        BinanceMarket::Spot,
        &serde_json::to_vec(&spot).expect("spot JSON"),
    )
    .expect("current Spot schema");
    assert_eq!(report.instruments().len(), 1);
    assert_eq!(report.skipped_unsupported_symbols(), 1);
    assert_eq!(report.skipped_unsupported_contracts(), 0);
    assert_eq!(report.skipped_inactive(), 1);
    assert_eq!(report.lifecycle().len(), 1);
    assert_eq!(report.lifecycle()[0].symbol(), "ETHUSDT");
    assert_eq!(report.lifecycle()[0].status(), "BREAK");
    assert_eq!(
        report.lifecycle()[0].kind(),
        BinanceInstrumentLifecycleKind::Inactive
    );

    let mut futures: serde_json::Value =
        serde_json::from_slice(USDM_EXCHANGE_INFO).expect("USD-M fixture");
    futures["futuresType"] = serde_json::json!("U_MARGINED");
    futures["symbols"][0]["permissionSets"] = serde_json::json!([["GRID"]]);
    futures["symbols"][0]["filters"]
        .as_array_mut()
        .expect("filters")
        .push(serde_json::json!({
            "filterType": "POSITION_RISK_CONTROL",
            "positionControlSide": "NONE"
        }));
    let mut quarterly = futures["symbols"][0].clone();
    quarterly["symbol"] = serde_json::json!("BTCUSDT_260925");
    quarterly["pair"] = serde_json::json!("BTCUSDT");
    quarterly["contractType"] = serde_json::json!("CURRENT_QUARTER");
    let mut settling = futures["symbols"][0].clone();
    settling["symbol"] = serde_json::json!("ETHUSDT");
    settling["pair"] = serde_json::json!("ETHUSDT");
    settling["baseAsset"] = serde_json::json!("ETH");
    settling["status"] = serde_json::json!("SETTLING");
    futures["symbols"]
        .as_array_mut()
        .expect("symbols")
        .extend([quarterly, settling]);

    let report = parse_exchange_info_report(
        BinanceMarket::UsdMarginedPerpetual,
        &serde_json::to_vec(&futures).expect("USD-M JSON"),
    )
    .expect("current USD-M schema");
    assert_eq!(report.instruments().len(), 1);
    assert_eq!(report.skipped_unsupported_symbols(), 0);
    assert_eq!(report.skipped_unsupported_contracts(), 1);
    assert_eq!(report.skipped_inactive(), 1);
    assert_eq!(report.lifecycle()[0].symbol(), "ETHUSDT");
    assert_eq!(report.lifecycle()[0].status(), "SETTLING");
    assert!(report.lifecycle()[0].onboard_time().is_some());
    assert!(report.lifecycle()[0].delivery_time().is_some());

    let mut pending = futures["symbols"][0].clone();
    pending["symbol"] = serde_json::json!("ETHUSDT");
    pending["pair"] = serde_json::json!("ETHUSDT");
    pending["baseAsset"] = serde_json::json!("ETH");
    pending["status"] = serde_json::json!("PENDING_TRADING");
    futures["symbols"]
        .as_array_mut()
        .expect("symbols")
        .push(pending);
    let report = parse_exchange_info_report(
        BinanceMarket::UsdMarginedPerpetual,
        &serde_json::to_vec(&futures).expect("pending JSON"),
    )
    .expect("pending schema");
    assert_eq!(report.instruments().len(), 1);
    assert_eq!(report.pending_instruments().len(), 1);
    assert_eq!(report.pending_instruments()[0].status(), "PENDING_TRADING");
    assert!(report.pending_instruments()[0].delivery_time().is_some());
}
