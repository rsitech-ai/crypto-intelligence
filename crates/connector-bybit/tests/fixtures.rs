use std::fs;
use std::num::{NonZeroU32, NonZeroU64};
use std::path::PathBuf;

use connector_bybit::{
    BookMessageKind, BybitInput, BybitMarket, BybitMessage, MAX_NATIVE_PAYLOAD_BYTES,
    NativeParseError, capabilities, parse_native_message,
};
use connector_core::{
    Completeness, ConnectionLifetime, MarketType, SequenceSemantics, SnapshotMethod, StreamClass,
};
use sha2::{Digest, Sha256};

fn fixture(name: &str) -> Vec<u8> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/exchanges/bybit")
        .join(name);
    fs::read(root).expect("retained fixture must be readable")
}

#[test]
fn retained_fixture_inventory_binds_every_payload_and_documentation_review() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/exchanges/bybit");
    let manifest = fs::read_to_string(root.join("manifest.toml")).expect("fixture manifest");
    let expected = [
        (
            "documentation-review.toml",
            "5c42ccdd487c4873cc69d0841c4b651731b8cf843c6e636381c344093a677d46",
        ),
        (
            "spot-orderbook-snapshot.json",
            "47ba0b138c88d1965b72173dcbb59e7142fa51fd01bc084bbf1727a9e1b3112d",
        ),
        (
            "spot-orderbook-delta.json",
            "c8d7e5ffa089f15bf683a709076b16f28d0ae58416202d19a482a61c7576f199",
        ),
        (
            "spot-orderbook-reset.json",
            "60a1018cac1496c85f4a4f58dc631537d7b473d240de8e9bc438bd3c2256a4bc",
        ),
        (
            "linear-orderbook-snapshot.json",
            "20f3eb7a27847ebf80712c3fbc2cb8044eb9cf1dc66ad84ffdb0d6beaaf3df6d",
        ),
        (
            "linear-orderbook-delta.json",
            "973ae6c2ad04d1351ff1197e9103db3067fc32e1fb98f45210e5c4ea5fcc1ead",
        ),
        (
            "spot-public-trade.json",
            "8fdcbdc1cbb0bfdb9b7255325e5674abcc78343c70155dfeae7956e39e9a4570",
        ),
        (
            "linear-public-trade.json",
            "33b0ee6a9f87cbe23ac6c4feb9c2c41c0863c671c0b5754eb4bfe56bc96fa99b",
        ),
        (
            "linear-ticker.json",
            "4055eb043d70a14bb0a22e5a437db0091eae4ce98f4858dd6b2ce66e5db01b63",
        ),
        (
            "linear-all-liquidation.json",
            "4e51771e3154c19a2c68d682c9d481cdb8bb67aa9620edbb9eac312c494d3c1a",
        ),
        (
            "spot-instruments-info.json",
            "c7581af9f8d1880e468e5f6dcad601b852c53b827017cbbd11571b6d273cc176",
        ),
        (
            "linear-instruments-info.json",
            "8525b4e98a3a0ba03fcf260d3ffd5583957e1d65f0eba71651af46e2c602d8f3",
        ),
    ];
    for (name, expected_sha256) in expected {
        let bytes = fs::read(root.join(name)).expect("retained artifact");
        let actual = format!("{:x}", Sha256::digest(bytes));
        assert_eq!(actual, expected_sha256, "{name} changed without review");
        assert!(
            manifest.contains(expected_sha256),
            "{name} digest is absent from the manifest"
        );
    }
    assert!(manifest.contains("captured_exchange_traffic = false"));
    assert!(manifest.contains("contains_credentials = false"));
    assert!(manifest.contains("delivery uncertainty"));
}

#[test]
fn current_capabilities_freeze_snapshot_reset_and_all_liquidation_semantics() {
    let profile = capabilities().expect("Bybit capabilities must be internally valid");
    let resetting = SequenceSemantics::SnapshotResettingUpdateId {
        reset_update_id: NonZeroU64::MIN,
        overwrite_on_every_snapshot: true,
    };
    assert_eq!(profile.snapshot_method(), SnapshotMethod::WebSocketSnapshot);
    assert_eq!(
        profile
            .book_sequence_semantics()
            .iter()
            .copied()
            .find(|rule| rule.market_type() == MarketType::Spot)
            .map(|rule| rule.semantics()),
        Some(resetting)
    );
    assert_eq!(
        profile
            .book_sequence_semantics()
            .iter()
            .copied()
            .find(|rule| rule.market_type() == MarketType::LinearPerpetual)
            .map(|rule| rule.semantics()),
        Some(resetting)
    );
    assert_eq!(
        profile.completeness().get(StreamClass::Liquidations),
        Completeness::VenueReportedAll {
            push_cadence_ms: NonZeroU32::new(500).expect("nonzero"),
            delivery_uncertainty: true,
        }
    );
    assert_eq!(
        profile.connection_lifetime(),
        ConnectionLifetime::VenueUnspecified
    );
}

#[test]
fn orderbook_snapshot_delta_and_service_reset_preserve_u_and_seq_independently() {
    let snapshot = parse_native_message(
        BybitInput::PublicWebSocket(BybitMarket::Spot),
        &fixture("spot-orderbook-snapshot.json"),
    )
    .expect("snapshot fixture must parse");
    let delta = parse_native_message(
        BybitInput::PublicWebSocket(BybitMarket::Spot),
        &fixture("spot-orderbook-delta.json"),
    )
    .expect("delta fixture must parse");
    let reset = parse_native_message(
        BybitInput::PublicWebSocket(BybitMarket::Spot),
        &fixture("spot-orderbook-reset.json"),
    )
    .expect("reset fixture must parse");

    let BybitMessage::OrderBook(snapshot) = snapshot else {
        panic!("expected order-book snapshot");
    };
    assert_eq!(snapshot.kind, BookMessageKind::Snapshot);
    assert_eq!(snapshot.update_id, 100);
    assert_eq!(snapshot.cross_sequence, 9_532_239_400);

    let BybitMessage::OrderBook(delta) = delta else {
        panic!("expected order-book delta");
    };
    assert_eq!(delta.kind, BookMessageKind::Delta);
    assert_eq!(delta.update_id, 101);
    assert_eq!(delta.cross_sequence, 9_532_239_402);

    let BybitMessage::OrderBook(reset) = reset else {
        panic!("expected order-book reset");
    };
    assert_eq!(reset.kind, BookMessageKind::Snapshot);
    assert_eq!(reset.update_id, 1);
    assert_eq!(reset.cross_sequence, 9_532_239_500);
}

#[test]
fn trade_batch_accepts_multiple_unique_trades_with_the_same_cross_sequence() {
    let message = parse_native_message(
        BybitInput::PublicWebSocket(BybitMarket::Spot),
        &fixture("spot-public-trade.json"),
    )
    .expect("trade fixture must parse");
    let BybitMessage::PublicTrades(trades) = message else {
        panic!("expected public trades");
    };
    assert_eq!(trades.len(), 2);
    assert_ne!(trades[0].trade_id, trades[1].trade_id);
    assert_eq!(trades[0].cross_sequence, trades[1].cross_sequence);
    assert!(!trades[0].rpi);
    assert!(trades[1].rpi);
}

#[test]
fn linear_ticker_and_all_liquidations_retain_current_official_fields() {
    let ticker = parse_native_message(
        BybitInput::PublicWebSocket(BybitMarket::LinearPerpetual),
        &fixture("linear-ticker.json"),
    )
    .expect("ticker fixture must parse");
    let BybitMessage::LinearTicker(ticker) = ticker else {
        panic!("expected linear ticker");
    };
    assert_eq!(ticker.symbol, "BTCUSDT");
    assert_eq!(ticker.mark_price.to_string(), "60002.8");
    assert_eq!(ticker.index_price.to_string(), "60001.5");
    assert_eq!(ticker.open_interest.to_string(), "492373.72");
    assert_eq!(ticker.funding_rate.to_string(), "-0.0005");

    let liquidations = parse_native_message(
        BybitInput::PublicWebSocket(BybitMarket::LinearPerpetual),
        &fixture("linear-all-liquidation.json"),
    )
    .expect("all-liquidation fixture must parse");
    let BybitMessage::AllLiquidations(liquidations) = liquidations else {
        panic!("expected liquidations");
    };
    assert_eq!(liquidations.len(), 2);
}

#[test]
fn wrong_market_route_duplicate_keys_and_oversized_input_fail_closed() {
    assert_eq!(
        parse_native_message(
            BybitInput::PublicWebSocket(BybitMarket::Spot),
            &fixture("linear-ticker.json"),
        ),
        Err(NativeParseError::WrongRoute)
    );
    assert_eq!(
        parse_native_message(
            BybitInput::PublicWebSocket(BybitMarket::Spot),
            br#"{"topic":"publicTrade.BTCUSDT","topic":"publicTrade.ETHUSDT","type":"snapshot","ts":1760325052200,"data":[]}"#,
        ),
        Err(NativeParseError::MalformedJson)
    );
    let oversized = vec![b' '; MAX_NATIVE_PAYLOAD_BYTES + 1];
    assert_eq!(
        parse_native_message(BybitInput::PublicWebSocket(BybitMarket::Spot), &oversized,),
        Err(NativeParseError::PayloadTooLarge)
    );
}

#[test]
fn nested_schema_drift_service_reset_delta_and_topic_mismatch_fail_closed() {
    let mut unknown: serde_json::Value =
        serde_json::from_slice(&fixture("spot-orderbook-snapshot.json")).expect("fixture JSON");
    unknown["data"]["undocumented"] = serde_json::json!(true);
    assert_eq!(
        parse_native_message(
            BybitInput::PublicWebSocket(BybitMarket::Spot),
            &serde_json::to_vec(&unknown).expect("JSON"),
        ),
        Err(NativeParseError::SchemaDrift)
    );

    let mut invalid_reset: serde_json::Value =
        serde_json::from_slice(&fixture("spot-orderbook-delta.json")).expect("fixture JSON");
    invalid_reset["data"]["u"] = serde_json::json!(1);
    assert_eq!(
        parse_native_message(
            BybitInput::PublicWebSocket(BybitMarket::Spot),
            &serde_json::to_vec(&invalid_reset).expect("JSON"),
        ),
        Err(NativeParseError::InvalidSequence)
    );

    let mut wrong_topic: serde_json::Value =
        serde_json::from_slice(&fixture("spot-orderbook-snapshot.json")).expect("fixture JSON");
    wrong_topic["topic"] = serde_json::json!("orderbook.50.ETHUSDT");
    assert_eq!(
        parse_native_message(
            BybitInput::PublicWebSocket(BybitMarket::Spot),
            &serde_json::to_vec(&wrong_topic).expect("JSON"),
        ),
        Err(NativeParseError::InvalidSymbol)
    );
}

#[test]
fn trade_collection_bound_is_enforced_before_normalization() {
    let mut value: serde_json::Value =
        serde_json::from_slice(&fixture("spot-public-trade.json")).expect("fixture JSON");
    let first = value["data"][0].clone();
    let trades = value["data"].as_array_mut().expect("trade array");
    trades.clear();
    for index in 0..=1_024_u64 {
        let mut trade = first.clone();
        trade["i"] = serde_json::json!(format!("bounded-trade-{index}"));
        trades.push(trade);
    }
    let raw = serde_json::to_vec(&value).expect("JSON");
    assert!(raw.len() < MAX_NATIVE_PAYLOAD_BYTES);
    assert_eq!(
        parse_native_message(BybitInput::PublicWebSocket(BybitMarket::Spot), &raw),
        Err(NativeParseError::CollectionTooLarge)
    );
}
