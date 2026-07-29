use connector_bybit::{
    BybitInstrumentLifecycle, BybitMarket, MetadataError, NativeParseError, parse_instruments_info,
};
use domain::ProductType;

const SPOT: &[u8] = include_bytes!("../../../fixtures/exchanges/bybit/spot-instruments-info.json");
const LINEAR: &[u8] =
    include_bytes!("../../../fixtures/exchanges/bybit/linear-instruments-info.json");

#[test]
fn spot_and_linear_metadata_use_authoritative_filter_increments() {
    let spot = parse_instruments_info(BybitMarket::Spot, SPOT).expect("spot metadata");
    assert_eq!(spot.market, BybitMarket::Spot);
    assert_eq!(spot.next_page_cursor, None);
    assert_eq!(spot.instruments.len(), 1);
    let spot = &spot.instruments[0];
    assert_eq!(spot.symbol_id, 9);
    assert_eq!(spot.product_type, ProductType::Spot);
    assert_eq!(spot.lifecycle, BybitInstrumentLifecycle::Active);
    assert_eq!(spot.settlement_asset, "USDT");
    assert_eq!(spot.price_tick.to_string(), "0.1");
    assert_eq!(spot.quantity_step.to_string(), "0.000001");
    assert_eq!(spot.listing_time, None);

    let linear =
        parse_instruments_info(BybitMarket::LinearPerpetual, LINEAR).expect("linear metadata");
    assert_eq!(linear.market, BybitMarket::LinearPerpetual);
    assert_eq!(linear.next_page_cursor, None);
    assert_eq!(linear.instruments.len(), 1);
    let linear = &linear.instruments[0];
    assert_eq!(linear.symbol_id, 5);
    assert_eq!(linear.product_type, ProductType::Perpetual);
    assert_eq!(linear.lifecycle, BybitInstrumentLifecycle::Active);
    assert_eq!(linear.settlement_asset, "USDT");
    assert_eq!(linear.price_tick.to_string(), "0.1");
    assert_eq!(linear.quantity_step.to_string(), "0.001");
    assert!(linear.listing_time.is_some());
    assert_eq!(linear.delisting_time, None);
}

#[test]
fn every_current_linear_lifecycle_is_retained_as_a_fact() {
    for (status, expected) in [
        ("PreLaunch", BybitInstrumentLifecycle::Pending),
        ("Trading", BybitInstrumentLifecycle::Active),
        ("Settling", BybitInstrumentLifecycle::Inactive),
        ("Delivering", BybitInstrumentLifecycle::Inactive),
        ("Closed", BybitInstrumentLifecycle::Inactive),
    ] {
        let mut value: serde_json::Value = serde_json::from_slice(LINEAR).expect("fixture JSON");
        value["result"]["list"][0]["status"] = serde_json::json!(status);
        let report = parse_instruments_info(
            BybitMarket::LinearPerpetual,
            &serde_json::to_vec(&value).expect("JSON"),
        )
        .expect("documented lifecycle must parse");
        assert_eq!(report.instruments[0].lifecycle, expected, "{status}");
    }
}

#[test]
fn wrong_category_unsupported_product_and_nested_schema_drift_fail_closed() {
    assert_eq!(
        parse_instruments_info(BybitMarket::Spot, LINEAR),
        Err(MetadataError::WrongCategory)
    );

    let mut unsupported: serde_json::Value = serde_json::from_slice(LINEAR).expect("fixture JSON");
    unsupported["result"]["list"][0]["contractType"] = serde_json::json!("LinearFutures");
    assert_eq!(
        parse_instruments_info(
            BybitMarket::LinearPerpetual,
            &serde_json::to_vec(&unsupported).expect("JSON"),
        ),
        Err(MetadataError::UnsupportedProduct)
    );

    let mut drift: serde_json::Value = serde_json::from_slice(SPOT).expect("fixture JSON");
    drift["result"]["list"][0]["priceFilter"]["undocumented"] = serde_json::json!("1");
    assert_eq!(
        parse_instruments_info(
            BybitMarket::Spot,
            &serde_json::to_vec(&drift).expect("JSON"),
        ),
        Err(MetadataError::Native(NativeParseError::SchemaDrift))
    );
}

#[test]
fn provider_failure_and_unknown_lifecycle_do_not_publish_definitions() {
    let mut failure: serde_json::Value = serde_json::from_slice(SPOT).expect("fixture JSON");
    failure["retCode"] = serde_json::json!(10006);
    failure["retMsg"] = serde_json::json!("Too many visits!");
    assert_eq!(
        parse_instruments_info(
            BybitMarket::Spot,
            &serde_json::to_vec(&failure).expect("JSON"),
        ),
        Err(MetadataError::ProviderFailure)
    );

    let mut unknown: serde_json::Value = serde_json::from_slice(LINEAR).expect("fixture JSON");
    unknown["result"]["list"][0]["status"] = serde_json::json!("UnknownFutureState");
    assert_eq!(
        parse_instruments_info(
            BybitMarket::LinearPerpetual,
            &serde_json::to_vec(&unknown).expect("JSON"),
        ),
        Err(MetadataError::UnsupportedLifecycle)
    );
}
