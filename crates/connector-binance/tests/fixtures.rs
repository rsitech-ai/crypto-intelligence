use connector_binance::{
    BinanceInput, BinanceMarket, BinanceMessage, NativeParseError, binance_capabilities,
    parse_depth_snapshot, parse_native_message,
};
use connector_core::{
    Completeness, ConnectionLifetime, FundingFields, MarketType, OpenInterestFields,
    SequenceSemantics, StreamClass, TradeSemantics,
};
use event_envelope::Side;

const SPOT_AGGREGATE_TRADE: &[u8] =
    include_bytes!("../../../fixtures/exchanges/binance/spot-aggregate-trade.json");
const SPOT_BOOK_TICKER: &[u8] =
    include_bytes!("../../../fixtures/exchanges/binance/spot-book-ticker.json");
const SPOT_DEPTH: &[u8] =
    include_bytes!("../../../fixtures/exchanges/binance/spot-depth-update.json");
const SPOT_DEPTH_SNAPSHOT: &[u8] =
    include_bytes!("../../../fixtures/exchanges/binance/spot-depth-snapshot.json");
const USDM_AGGREGATE_TRADE: &[u8] =
    include_bytes!("../../../fixtures/exchanges/binance/usdm-aggregate-trade.json");
const USDM_DEPTH: &[u8] =
    include_bytes!("../../../fixtures/exchanges/binance/usdm-depth-update.json");
const USDM_DEPTH_SNAPSHOT: &[u8] =
    include_bytes!("../../../fixtures/exchanges/binance/usdm-depth-snapshot.json");
const USDM_MARK_PRICE: &[u8] =
    include_bytes!("../../../fixtures/exchanges/binance/usdm-mark-price.json");
const USDM_OPEN_INTEREST: &[u8] =
    include_bytes!("../../../fixtures/exchanges/binance/usdm-open-interest.json");
const USDM_LIQUIDATION: &[u8] =
    include_bytes!("../../../fixtures/exchanges/binance/usdm-liquidation.json");
const SYSTEM_STATUS: &[u8] =
    include_bytes!("../../../fixtures/exchanges/binance/system-status.json");

#[test]
fn every_certified_native_fixture_has_exactly_one_parser_route() {
    let inputs = [
        BinanceInput::SpotWebSocket,
        BinanceInput::UsdMAggregateTradeWebSocket,
        BinanceInput::UsdMMarkPriceWebSocket,
        BinanceInput::UsdMLiquidationWebSocket,
        BinanceInput::UsdMDepthWebSocket,
        BinanceInput::UsdMOpenInterestRest,
        BinanceInput::SystemStatusRest,
    ];
    for (payload, expected) in [
        (SPOT_AGGREGATE_TRADE, BinanceInput::SpotWebSocket),
        (SPOT_BOOK_TICKER, BinanceInput::SpotWebSocket),
        (SPOT_DEPTH, BinanceInput::SpotWebSocket),
        (
            USDM_AGGREGATE_TRADE,
            BinanceInput::UsdMAggregateTradeWebSocket,
        ),
        (USDM_MARK_PRICE, BinanceInput::UsdMMarkPriceWebSocket),
        (USDM_LIQUIDATION, BinanceInput::UsdMLiquidationWebSocket),
        (USDM_DEPTH, BinanceInput::UsdMDepthWebSocket),
        (USDM_OPEN_INTEREST, BinanceInput::UsdMOpenInterestRest),
        (SYSTEM_STATUS, BinanceInput::SystemStatusRest),
    ] {
        let accepted = inputs
            .iter()
            .copied()
            .filter(|input| parse_native_message(*input, payload).is_ok())
            .collect::<Vec<_>>();
        assert_eq!(
            accepted,
            vec![expected],
            "provider payload must not be relabelable across parser routes"
        );
    }
}

#[test]
fn connector_owned_capabilities_are_truthful_and_market_specific() {
    let capabilities = binance_capabilities().expect("Binance capabilities must validate");

    assert_eq!(
        capabilities.market_types(),
        &[MarketType::Spot, MarketType::LinearPerpetual]
    );
    assert_eq!(capabilities.trade_semantics(), TradeSemantics::Aggregate);
    assert_eq!(
        capabilities.connection_lifetime(),
        ConnectionLifetime::Finite {
            lifetime_ms: core::num::NonZeroU32::new(86_400_000).expect("nonzero"),
            renewal_margin_ms: core::num::NonZeroU32::new(300_000).expect("nonzero"),
        }
    );
    assert_eq!(
        capabilities
            .book_sequence_semantics()
            .iter()
            .map(|rule| rule.semantics())
            .collect::<Vec<_>>(),
        vec![
            SequenceSemantics::InclusiveRange,
            SequenceSemantics::InclusiveRangeWithPreviousFinal,
        ]
    );
    assert_eq!(
        capabilities.completeness().get(StreamClass::Liquidations),
        Completeness::SampledLargestPerSymbolWindow {
            window_ms: core::num::NonZeroU32::new(1_000).expect("nonzero"),
        }
    );
    assert_eq!(
        capabilities.funding_fields().bits(),
        FundingFields::RATE.bits() | FundingFields::NEXT_FUNDING_TIME.bits()
    );
    assert_eq!(
        capabilities.open_interest_fields(),
        OpenInterestFields::VALUE
    );
    assert!(
        capabilities
            .rate_limit_model()
            .iter()
            .any(|rule| rule.applies_to() == "spot_stream_control")
    );
    assert!(
        capabilities
            .rate_limit_model()
            .iter()
            .any(|rule| rule.applies_to() == "usd_m_stream_control")
    );
}

#[test]
fn spot_native_fixtures_preserve_exact_wire_semantics() {
    let BinanceMessage::AggregateTrade(trade) =
        parse_native_message(BinanceInput::SpotWebSocket, SPOT_AGGREGATE_TRADE)
            .expect("spot aggregate trade")
    else {
        panic!("wrong spot aggregate-trade variant");
    };
    assert_eq!(trade.market, BinanceMarket::Spot);
    assert_eq!(trade.symbol, "BTCUSDT");
    assert_eq!(trade.aggregate_trade_id, 5_933_014);
    assert_eq!(trade.price.to_string(), "16695.14");
    assert_eq!(trade.quantity.to_string(), "0.005");
    assert_eq!(trade.first_trade_id, 100);
    assert_eq!(trade.last_trade_id, 105);
    assert!(trade.buyer_is_market_maker);
    assert_eq!(trade.market_category, None);

    let BinanceMessage::BookTicker(ticker) =
        parse_native_message(BinanceInput::SpotWebSocket, SPOT_BOOK_TICKER)
            .expect("spot book ticker")
    else {
        panic!("wrong spot book-ticker variant");
    };
    assert_eq!(ticker.update_id, 400_900_217);
    assert_eq!(ticker.bid_price.to_string(), "16695.13");
    assert_eq!(ticker.ask_price.to_string(), "16695.14");

    let BinanceMessage::DepthUpdate(depth) =
        parse_native_message(BinanceInput::SpotWebSocket, SPOT_DEPTH).expect("spot depth")
    else {
        panic!("wrong spot depth variant");
    };
    assert_eq!(depth.market, BinanceMarket::Spot);
    assert_eq!(depth.first_update_id, 157);
    assert_eq!(depth.final_update_id, 160);
    assert_eq!(depth.previous_final_update_id, None);
    assert_eq!(depth.bids.len(), 2);
    assert_eq!(depth.bids[1].quantity.to_string(), "0");
}

#[test]
fn usd_m_native_fixtures_preserve_routes_category_and_previous_final() {
    let BinanceMessage::AggregateTrade(trade) = parse_native_message(
        BinanceInput::UsdMAggregateTradeWebSocket,
        USDM_AGGREGATE_TRADE,
    )
    .expect("USD-M aggregate trade") else {
        panic!("wrong USD-M aggregate-trade variant");
    };
    assert_eq!(trade.market, BinanceMarket::UsdMarginedPerpetual);
    assert_eq!(trade.market_category, Some(1));
    assert_eq!(
        trade.normal_quantity.expect("normal quantity").to_string(),
        "0.004"
    );

    let BinanceMessage::DepthUpdate(depth) =
        parse_native_message(BinanceInput::UsdMDepthWebSocket, USDM_DEPTH).expect("USD-M depth")
    else {
        panic!("wrong USD-M depth variant");
    };
    assert_eq!(depth.market, BinanceMarket::UsdMarginedPerpetual);
    assert_eq!(depth.first_update_id, 157);
    assert_eq!(depth.final_update_id, 160);
    assert_eq!(depth.previous_final_update_id, Some(149));
    assert_eq!(
        depth.transaction_time.expect("transaction time").value(),
        1_672_515_782_135_000_000
    );

    let BinanceMessage::MarkPrice(mark) =
        parse_native_message(BinanceInput::UsdMMarkPriceWebSocket, USDM_MARK_PRICE)
            .expect("USD-M mark price")
    else {
        panic!("wrong USD-M mark-price variant");
    };
    assert_eq!(mark.mark_price.to_string(), "16694.77");
    assert_eq!(mark.index_price.to_string(), "16693.54");
    assert_eq!(mark.funding_rate.to_string(), "0.0001");
    assert_eq!(mark.moving_average_price.to_string(), "16694.81");
    assert_eq!(mark.next_funding_time.value(), 1_672_531_200_000_000_000);

    let BinanceMessage::OpenInterest(open_interest) =
        parse_native_message(BinanceInput::UsdMOpenInterestRest, USDM_OPEN_INTEREST)
            .expect("USD-M open interest")
    else {
        panic!("wrong USD-M open-interest variant");
    };
    assert_eq!(open_interest.quantity.to_string(), "10659.509");

    let BinanceMessage::Liquidation(liquidation) =
        parse_native_message(BinanceInput::UsdMLiquidationWebSocket, USDM_LIQUIDATION)
            .expect("USD-M liquidation")
    else {
        panic!("wrong USD-M liquidation variant");
    };
    assert_eq!(liquidation.market_category, 1);
    assert_eq!(liquidation.pair_symbol, "BTCUSDT");
    assert_eq!(liquidation.side, Side::Sell);
    assert_eq!(liquidation.filled_quantity.to_string(), "0.014");
    assert_eq!(liquidation.average_price.to_string(), "16689.8");
}

#[test]
fn parser_accepts_field_order_but_rejects_undocumented_schema_drift() {
    let reordered = br#"{"M":true,"m":true,"T":1672515782136,"l":105,"f":100,"q":"0.005","p":"16695.14","a":5933014,"s":"BTCUSDT","E":1672515782136,"e":"aggTrade"}"#;
    assert!(matches!(
        parse_native_message(BinanceInput::SpotWebSocket, reordered),
        Ok(BinanceMessage::AggregateTrade(_))
    ));

    let with_unknown = br#"{"e":"aggTrade","E":1672515782136,"s":"BTCUSDT","a":5933014,"p":"16695.14","q":"0.005","f":100,"l":105,"T":1672515782136,"m":true,"M":true,"silent_new_field":"danger"}"#;
    assert_eq!(
        parse_native_message(BinanceInput::SpotWebSocket, with_unknown),
        Err(NativeParseError::SchemaDrift)
    );
}

#[test]
fn parser_rejects_wrong_route_market_category_numeric_timestamp_and_size_boundaries() {
    assert_eq!(
        parse_native_message(BinanceInput::UsdMMarkPriceWebSocket, USDM_DEPTH),
        Err(NativeParseError::WrongRoute)
    );
    assert_eq!(
        parse_native_message(BinanceInput::UsdMDepthWebSocket, USDM_MARK_PRICE),
        Err(NativeParseError::WrongRoute)
    );

    let coin_m = String::from_utf8(USDM_DEPTH.to_vec())
        .expect("fixture utf8")
        .replacen("\"st\": 1", "\"st\": 2", 1);
    assert_eq!(
        parse_native_message(BinanceInput::UsdMDepthWebSocket, coin_m.as_bytes()),
        Err(NativeParseError::UnsupportedMarketCategory)
    );

    let provider_scale = String::from_utf8(SPOT_AGGREGATE_TRADE.to_vec())
        .expect("fixture utf8")
        .replacen("\"16695.14\"", "\"16695.140\"", 1);
    let BinanceMessage::AggregateTrade(provider_scale) =
        parse_native_message(BinanceInput::SpotWebSocket, provider_scale.as_bytes())
            .expect("provider decimal scale is canonicalized")
    else {
        panic!("aggregate trade");
    };
    assert_eq!(provider_scale.price.to_string(), "16695.14");

    let invalid_decimal = String::from_utf8(SPOT_AGGREGATE_TRADE.to_vec())
        .expect("fixture utf8")
        .replacen("\"16695.14\"", "\"16695.14e0\"", 1);
    assert_eq!(
        parse_native_message(BinanceInput::SpotWebSocket, invalid_decimal.as_bytes()),
        Err(NativeParseError::InvalidDecimal)
    );

    let confused_timestamp = String::from_utf8(SPOT_AGGREGATE_TRADE.to_vec())
        .expect("fixture utf8")
        .replacen("1672515782136", "1672515782", 1);
    assert_eq!(
        parse_native_message(BinanceInput::SpotWebSocket, confused_timestamp.as_bytes()),
        Err(NativeParseError::InvalidTimestamp)
    );

    let oversized = vec![b' '; connector_binance::MAX_NATIVE_PAYLOAD_BYTES + 1];
    assert_eq!(
        parse_native_message(BinanceInput::SpotWebSocket, &oversized),
        Err(NativeParseError::PayloadTooLarge)
    );
}

#[test]
fn parser_rejects_missing_required_duplicate_and_inconsistent_fields() {
    let missing_spot_best_match = br#"{"e":"aggTrade","E":1672515782136,"s":"BTCUSDT","a":5933014,"p":"16695.14","q":"0.005","f":100,"l":105,"T":1672515782136,"m":true}"#;
    assert_eq!(
        parse_native_message(BinanceInput::SpotWebSocket, missing_spot_best_match),
        Err(NativeParseError::MalformedJson)
    );

    let missing_futures_normal_quantity = br#"{"e":"aggTrade","E":1672515782136,"s":"BTCUSDT","a":5933014,"p":"16695.14","q":"0.005","f":100,"l":105,"T":1672515782136,"m":true,"st":1}"#;
    assert_eq!(
        parse_native_message(
            BinanceInput::UsdMAggregateTradeWebSocket,
            missing_futures_normal_quantity
        ),
        Err(NativeParseError::MalformedJson)
    );

    let duplicate_price = br#"{"e":"aggTrade","E":1672515782136,"s":"BTCUSDT","a":5933014,"p":"1","p":"16695.14","q":"0.005","f":100,"l":105,"T":1672515782136,"m":true,"M":true}"#;
    assert_eq!(
        parse_native_message(BinanceInput::SpotWebSocket, duplicate_price),
        Err(NativeParseError::MalformedJson)
    );

    let missing_futures_pair = String::from_utf8(USDM_DEPTH.to_vec())
        .expect("fixture utf8")
        .replacen("\"ps\": \"BTCUSDT\",\n  ", "", 1);
    assert_eq!(
        parse_native_message(
            BinanceInput::UsdMDepthWebSocket,
            missing_futures_pair.as_bytes()
        ),
        Err(NativeParseError::MalformedJson)
    );

    let funding_before_observation = String::from_utf8(USDM_MARK_PRICE.to_vec())
        .expect("fixture utf8")
        .replacen("1672531200000", "1672515782135", 1);
    assert_eq!(
        parse_native_message(
            BinanceInput::UsdMMarkPriceWebSocket,
            funding_before_observation.as_bytes()
        ),
        Err(NativeParseError::InvalidTimestamp)
    );
}

#[test]
fn parser_bounds_each_depth_side_before_normalization() {
    let levels = (0..5_001)
        .map(|index| {
            let price = format!("{}.{:03}", 10_000 + index, 1);
            serde_json::json!([price, "1"])
        })
        .collect::<Vec<_>>();
    let payload = serde_json::to_vec(&serde_json::json!({
        "e": "depthUpdate",
        "E": 1672515782136_u64,
        "s": "BTCUSDT",
        "U": 157_u64,
        "u": 160_u64,
        "b": levels,
        "a": [["20000.001", "1"]]
    }))
    .expect("bounded test JSON");
    assert!(payload.len() < connector_binance::MAX_NATIVE_PAYLOAD_BYTES);
    assert_eq!(
        parse_native_message(BinanceInput::SpotWebSocket, &payload),
        Err(NativeParseError::TooManyLevels)
    );
}

#[test]
fn retained_rest_snapshots_preserve_spot_and_usd_m_timestamp_contracts() {
    let spot =
        parse_depth_snapshot(BinanceMarket::Spot, "BTCUSDT", SPOT_DEPTH_SNAPSHOT).expect("spot");
    assert_eq!(spot.last_update_id, 156);
    assert_eq!(spot.event_time, None);
    assert_eq!(spot.transaction_time, None);

    let futures = parse_depth_snapshot(
        BinanceMarket::UsdMarginedPerpetual,
        "BTCUSDT",
        USDM_DEPTH_SNAPSHOT,
    )
    .expect("USD-M");
    assert_eq!(futures.last_update_id, 156);
    assert!(futures.event_time.is_some());
    assert!(futures.transaction_time.is_some());
}
