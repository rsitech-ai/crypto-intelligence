use connector_core::{
    ChecksumSupport, DurableRawCaptureChannel, RateLimitValue, RawCapture, SequenceSemantics,
    SourceTimestampPrecision, wal_stream_source_identity,
};
use connector_kraken::{
    AssetStatus, BookMessageKind, KrakenBookSynchronizer, KrakenInput, KrakenL3Policy,
    KrakenMessage, MAX_NATIVE_PAYLOAD_BYTES, NativeBookLevel, NativeParseError, PairStatus,
    SystemState, kraken_crc32, kraken_l3_crc32, parse_durable_native_message, parse_native_message,
};
use domain::{InstrumentId, SourceId, SourceKind, UnixNanos, VenueId};
use fixed_decimal::{FixedDecimal, Price, Quantity};
use orderbook::{
    ApplyResult, BookConfig, BookSession, BookState, ChecksumPolicy, ExactChecksumLevel,
    ExactL3ChecksumOrder, L3Order, L3OrderId, L3Side, SequencePolicy,
};
use raw_wal::{
    frame::RecordMetadata,
    manager::{RotationPolicy, SegmentedWalWriter},
    prologue::{SegmentMetadata, StreamDescriptor},
};
use std::num::{NonZeroU32, NonZeroU64};

const KRAKEN_BIDS: [(&str, &str); 10] = [
    ("45283.5", "0.10000000"),
    ("45283.4", "1.54582015"),
    ("45282.1", "0.10000000"),
    ("45281.0", "0.10000000"),
    ("45280.3", "1.54592586"),
    ("45279.0", "0.07990000"),
    ("45277.6", "0.03310103"),
    ("45277.5", "0.30000000"),
    ("45277.3", "1.54602737"),
    ("45276.6", "0.15445238"),
];
const KRAKEN_ASKS: [(&str, &str); 10] = [
    ("45285.2", "0.00100000"),
    ("45286.4", "1.54571953"),
    ("45286.6", "1.54571109"),
    ("45289.6", "1.54560911"),
    ("45290.2", "0.15890660"),
    ("45291.8", "1.54553491"),
    ("45294.7", "0.04454749"),
    ("45296.1", "0.35380000"),
    ("45297.5", "0.09945542"),
    ("45299.5", "0.18772827"),
];

#[test]
fn checksum_matches_current_official_v2_reference() {
    assert_eq!(
        kraken_crc32(exact_levels(&KRAKEN_ASKS), exact_levels(&KRAKEN_BIDS)).unwrap(),
        3_310_070_434
    );
}

#[test]
fn parser_preserves_decimal_lexemes_needed_by_checksum() {
    let raw = include_bytes!("../../../fixtures/exchanges/kraken/spot-book-snapshot.json");
    let KrakenMessage::SpotBook(message) =
        parse_native_message(KrakenInput::SpotWebSocketV2, raw).unwrap()
    else {
        panic!("expected Spot book");
    };
    assert_eq!(message.kind, BookMessageKind::Snapshot);
    assert_eq!(message.bids[0].quantity_text, "0.10000000");
    assert_eq!(message.asks[0].quantity_text, "0.00100000");
    assert_eq!(message.checksum, 3_310_070_434);
}

#[test]
fn checksum_mismatch_never_publishes_book_and_requires_snapshot() {
    let raw = include_bytes!("../../../fixtures/exchanges/kraken/spot-book-snapshot.json");
    let KrakenMessage::SpotBook(valid) =
        parse_native_message(KrakenInput::SpotWebSocketV2, raw).unwrap()
    else {
        panic!("expected Spot book");
    };
    let mut bad = valid.clone();
    bad.checksum = bad.checksum.wrapping_add(1);

    let mut sync = KrakenBookSynchronizer::try_new(book_config()).unwrap();
    sync.start_session(session()).unwrap();
    assert_eq!(
        sync.apply_spot(&bad, 1, 7).unwrap(),
        ApplyResult::ChecksumMismatch
    );
    assert_eq!(sync.state(), BookState::Untrusted);
    assert!(sync.snapshot().is_err());

    assert_eq!(sync.apply_spot(&valid, 2, 8).unwrap(), ApplyResult::Applied);
    assert_eq!(sync.state(), BookState::Synchronized);
    assert_eq!(sync.snapshot().unwrap().bids().len(), 10);
}

#[test]
fn repeated_l2_price_updates_are_applied_in_message_order() {
    let raw = include_bytes!("../../../fixtures/exchanges/kraken/spot-book-snapshot.json");
    let KrakenMessage::SpotBook(snapshot) =
        parse_native_message(KrakenInput::SpotWebSocketV2, raw).unwrap()
    else {
        panic!("expected Spot book");
    };
    let mut changed_bids = KRAKEN_BIDS.to_vec();
    changed_bids[0].1 = "0.30000000";
    let update = connector_kraken::SpotBookMessage {
        symbol: snapshot.symbol.clone(),
        kind: BookMessageKind::Update,
        timestamp: "2026-07-29T12:00:01.000000Z".to_owned(),
        checksum: kraken_crc32(exact_levels(&KRAKEN_ASKS), exact_levels(&changed_bids)).unwrap(),
        bids: vec![
            native_level("45283.5", "0.20000000"),
            native_level("45283.5", "0.30000000"),
        ],
        asks: Vec::new(),
    };

    let mut sync = KrakenBookSynchronizer::try_new(book_config()).unwrap();
    sync.start_session(session()).unwrap();
    assert_eq!(
        sync.apply_spot(&snapshot, 1, 7).unwrap(),
        ApplyResult::Applied
    );
    assert_eq!(
        sync.apply_spot(&update, 2, 8).unwrap(),
        ApplyResult::Applied
    );
    assert_eq!(
        sync.snapshot().unwrap().bids()[0].quantity,
        quantity("0.30000000")
    );
}

#[test]
fn l3_checksum_includes_order_queue_and_policy_is_explicit() {
    let asks = vec![
        exact_l3("ASK-1", L3Side::Ask, "100.1", "0.01000000", 1),
        exact_l3("ASK-2", L3Side::Ask, "100.1", "0.02000000", 2),
    ];
    let bids = vec![exact_l3("BID-1", L3Side::Bid, "100.0", "0.03000000", 1)];
    assert_eq!(kraken_l3_crc32(asks, bids).unwrap(), 776_282_110);

    assert!(!KrakenL3Policy::disabled().is_enabled_for("BTC/USD"));
    let policy = KrakenL3Policy::try_new(["BTC/USD".to_owned()], 10, 5_000).unwrap();
    assert!(policy.is_enabled_for("BTC/USD"));
    assert!(!policy.is_enabled_for("ETH/USD"));
    assert_eq!(policy.symbols().collect::<Vec<_>>(), ["BTC/USD"]);
}

#[test]
fn selected_l3_snapshot_preserves_queue_and_checksum_mismatch_suppresses_it() {
    let book_raw = include_bytes!("../../../fixtures/exchanges/kraken/spot-book-snapshot.json");
    let KrakenMessage::SpotBook(book) =
        parse_native_message(KrakenInput::SpotWebSocketV2, book_raw).unwrap()
    else {
        panic!("expected Spot book");
    };
    let l3_raw = include_bytes!("../../../fixtures/exchanges/kraken/spot-level3-snapshot.json");
    let KrakenMessage::SpotLevel3(l3) =
        parse_native_message(KrakenInput::SpotWebSocketV2, l3_raw).unwrap()
    else {
        panic!("expected Spot L3");
    };
    let policy = KrakenL3Policy::try_new(["BTC/USD".to_owned()], 10, 5_000).unwrap();
    let mut sync = KrakenBookSynchronizer::try_new_with_l3(l3_book_config(), policy).unwrap();
    sync.start_session(session()).unwrap();
    assert_eq!(sync.apply_spot(&book, 1, 7).unwrap(), ApplyResult::Applied);
    assert_eq!(sync.apply_level3(&l3, 2, 8).unwrap(), ApplyResult::Applied);
    let snapshot = sync.snapshot().unwrap();
    let original_queue = snapshot
        .l3_orders()
        .iter()
        .map(|order| {
            (
                order.id().as_str().to_owned(),
                order.priority_ns(),
                order.queue_order(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        snapshot
            .l3_orders()
            .iter()
            .map(|order| order.id().as_str())
            .collect::<Vec<_>>(),
        ["BID-1", "ASK-1", "ASK-2"]
    );

    let mut bad = l3.clone();
    bad.checksum = bad.checksum.wrapping_add(1);
    assert_eq!(
        sync.apply_level3(&bad, 3, 9).unwrap(),
        ApplyResult::ChecksumMismatch
    );
    assert!(sync.snapshot().unwrap().l3_orders().is_empty());
    assert_eq!(sync.apply_level3(&l3, 4, 10).unwrap(), ApplyResult::Applied);
    assert_eq!(
        sync.snapshot()
            .unwrap()
            .l3_orders()
            .iter()
            .map(|order| {
                (
                    order.id().as_str().to_owned(),
                    order.priority_ns(),
                    order.queue_order(),
                )
            })
            .collect::<Vec<_>>(),
        original_queue
    );
}

#[test]
fn all_retained_message_shapes_parse_on_their_documented_route() {
    let spot_cases = [
        include_bytes!("../../../fixtures/exchanges/kraken/spot-level3-snapshot.json").as_slice(),
        include_bytes!("../../../fixtures/exchanges/kraken/spot-trade.json").as_slice(),
        include_bytes!("../../../fixtures/exchanges/kraken/spot-instrument.json").as_slice(),
        include_bytes!("../../../fixtures/exchanges/kraken/spot-status.json").as_slice(),
        include_bytes!("../../../fixtures/exchanges/kraken/spot-heartbeat.json").as_slice(),
    ];
    for raw in spot_cases {
        parse_native_message(KrakenInput::SpotWebSocketV2, raw).unwrap();
    }

    let futures_cases = [
        include_bytes!("../../../fixtures/exchanges/kraken/futures-book-snapshot.json").as_slice(),
        include_bytes!("../../../fixtures/exchanges/kraken/futures-book-update.json").as_slice(),
        include_bytes!("../../../fixtures/exchanges/kraken/futures-trade.json").as_slice(),
    ];
    for raw in futures_cases {
        parse_native_message(KrakenInput::FuturesWebSocket, raw).unwrap();
    }

    let status = parse_native_message(
        KrakenInput::SpotWebSocketV2,
        include_bytes!("../../../fixtures/exchanges/kraken/spot-status.json"),
    )
    .unwrap();
    assert!(matches!(
        status,
        KrakenMessage::SpotStatus {
            state: SystemState::Online,
            ..
        }
    ));
    let instrument = parse_native_message(
        KrakenInput::SpotWebSocketV2,
        include_bytes!("../../../fixtures/exchanges/kraken/spot-instrument.json"),
    )
    .unwrap();
    let KrakenMessage::SpotInstrument(instrument) = instrument else {
        panic!("expected Spot instrument metadata");
    };
    assert_eq!(instrument.kind, BookMessageKind::Snapshot);
    assert_eq!(instrument.assets[0].status, AssetStatus::Enabled);
    assert_eq!(instrument.pairs[0].status, PairStatus::Online);
    assert_eq!(instrument.pairs[0].price_increment, price("0.1"));
    assert_eq!(
        instrument.pairs[0].quantity_increment,
        quantity("0.00000001")
    );
}

#[test]
fn parser_rejects_duplicate_keys_unknown_fields_wrong_routes_and_oversize() {
    let duplicate = br#"{"channel":"heartbeat","channel":"heartbeat"}"#;
    assert_eq!(
        parse_native_message(KrakenInput::SpotWebSocketV2, duplicate),
        Err(NativeParseError::MalformedJson)
    );
    let drift = br#"{"channel":"heartbeat","unexpected":true}"#;
    assert_eq!(
        parse_native_message(KrakenInput::SpotWebSocketV2, drift),
        Err(NativeParseError::SchemaDrift)
    );
    let futures = include_bytes!("../../../fixtures/exchanges/kraken/futures-book-snapshot.json");
    assert!(parse_native_message(KrakenInput::SpotWebSocketV2, futures).is_err());
    let oversized = vec![b' '; MAX_NATIVE_PAYLOAD_BYTES + 1];
    assert_eq!(
        parse_native_message(KrakenInput::SpotWebSocketV2, &oversized),
        Err(NativeParseError::PayloadTooLarge)
    );
    let mut invalid_timestamp = serde_json::from_slice::<serde_json::Value>(include_bytes!(
        "../../../fixtures/exchanges/kraken/spot-trade.json"
    ))
    .unwrap();
    invalid_timestamp["data"][0]["timestamp"] = serde_json::json!("not-a-real-timestampZ");
    assert_eq!(
        parse_native_message(
            KrakenInput::SpotWebSocketV2,
            &serde_json::to_vec(&invalid_timestamp).unwrap(),
        ),
        Err(NativeParseError::InvalidValue)
    );
    let mut zero_futures_timestamp = serde_json::from_slice::<serde_json::Value>(include_bytes!(
        "../../../fixtures/exchanges/kraken/futures-book-update.json"
    ))
    .unwrap();
    zero_futures_timestamp["timestamp"] = serde_json::json!(0);
    assert_eq!(
        parse_native_message(
            KrakenInput::FuturesWebSocket,
            &serde_json::to_vec(&zero_futures_timestamp).unwrap(),
        ),
        Err(NativeParseError::InvalidValue)
    );
}

#[test]
fn instrument_metadata_rejects_unknown_lifecycle_and_ambiguous_identity() {
    let raw = include_bytes!("../../../fixtures/exchanges/kraken/spot-instrument.json");
    let mut unknown_status = serde_json::from_slice::<serde_json::Value>(raw).unwrap();
    unknown_status["data"]["pairs"][0]["status"] = serde_json::json!("unknown");
    assert_eq!(
        parse_native_message(
            KrakenInput::SpotWebSocketV2,
            &serde_json::to_vec(&unknown_status).unwrap(),
        ),
        Err(NativeParseError::InvalidValue)
    );

    let mut duplicate_pair = serde_json::from_slice::<serde_json::Value>(raw).unwrap();
    let pair = duplicate_pair["data"]["pairs"][0].clone();
    duplicate_pair["data"]["pairs"]
        .as_array_mut()
        .unwrap()
        .push(pair);
    assert_eq!(
        parse_native_message(
            KrakenInput::SpotWebSocketV2,
            &serde_json::to_vec(&duplicate_pair).unwrap(),
        ),
        Err(NativeParseError::InvalidValue)
    );

    let mut zero_increment = serde_json::from_slice::<serde_json::Value>(raw).unwrap();
    zero_increment["data"]["pairs"][0]["price_increment"] = serde_json::json!("0");
    assert_eq!(
        parse_native_message(
            KrakenInput::SpotWebSocketV2,
            &serde_json::to_vec(&zero_increment).unwrap(),
        ),
        Err(NativeParseError::InvalidDecimal)
    );

    let mut unknown_asset = serde_json::from_slice::<serde_json::Value>(raw).unwrap();
    unknown_asset["data"]["pairs"][0]["base"] = serde_json::json!("MISSING");
    assert_eq!(
        parse_native_message(
            KrakenInput::SpotWebSocketV2,
            &serde_json::to_vec(&unknown_asset).unwrap(),
        ),
        Err(NativeParseError::InvalidValue)
    );
}

#[test]
fn capabilities_do_not_invent_spot_sequence_or_liquidation_coverage() {
    let capabilities = connector_kraken::capabilities().unwrap();
    assert_eq!(
        capabilities.checksum_support(),
        ChecksumSupport::Crc32DecimalBook
    );
    assert!(
        capabilities
            .book_sequence_semantics()
            .iter()
            .any(|rule| { rule.semantics() == SequenceSemantics::ChecksumValidatedNoSequence })
    );
    assert_eq!(
        capabilities.liquidation_completeness(),
        connector_core::Completeness::NotSupported
    );
    assert!(matches!(
        capabilities
            .completeness()
            .get(connector_core::StreamClass::InstrumentDefinitions),
        connector_core::Completeness::VenueReportedComplete { .. }
    ));
    assert_eq!(
        capabilities.source_timestamp_precision(),
        SourceTimestampPrecision::Milliseconds
    );
    assert_eq!(
        capabilities
            .rate_limit_model()
            .iter()
            .map(|rule| {
                (
                    rule.applies_to(),
                    rule.limit(),
                    rule.request_cost().map(NonZeroU32::get),
                )
            })
            .collect::<Vec<_>>(),
        [
            (
                "spot_level3_depth_10_standard_subscription_counter",
                RateLimitValue::Fixed(NonZeroU32::new(200).unwrap()),
                Some(5),
            ),
            (
                "spot_level3_depth_100_standard_subscription_counter",
                RateLimitValue::Fixed(NonZeroU32::new(200).unwrap()),
                Some(25),
            ),
            (
                "spot_level3_depth_1000_standard_subscription_counter",
                RateLimitValue::Fixed(NonZeroU32::new(200).unwrap()),
                Some(100),
            ),
        ]
    );
}

#[tokio::test]
async fn durable_parser_rejects_payload_not_bound_to_wal_receipt() {
    let payload = include_bytes!("../../../fixtures/exchanges/kraken/spot-trade.json");
    let reference = durable_reference(payload).await;
    let parsed =
        parse_durable_native_message(KrakenInput::SpotWebSocketV2, payload, &reference).unwrap();
    assert_eq!(parsed.raw_payload_hash(), reference.payload_hash());

    let other = include_bytes!("../../../fixtures/exchanges/kraken/spot-heartbeat.json");
    assert_eq!(
        parse_durable_native_message(KrakenInput::SpotWebSocketV2, other, &reference),
        Err(NativeParseError::RawPayloadMismatch)
    );
}

fn exact_levels(values: &[(&str, &str)]) -> Vec<ExactChecksumLevel> {
    values
        .iter()
        .map(|(price, quantity)| ExactChecksumLevel::new(*price, *quantity).unwrap())
        .collect()
}

fn exact_l3(
    id: &str,
    side: L3Side,
    price_value: &str,
    quantity_value: &str,
    queue_order: u64,
) -> ExactL3ChecksumOrder {
    let order = L3Order::new(
        L3OrderId::new(id).unwrap(),
        side,
        price(price_value),
        quantity(quantity_value),
        queue_order,
        queue_order,
    )
    .unwrap();
    ExactL3ChecksumOrder::new(order, price_value, quantity_value).unwrap()
}

fn book_config() -> BookConfig {
    BookConfig {
        instrument: InstrumentId::new(VenueId::new("kraken").unwrap(), "BTCUSD", 1).unwrap(),
        price_tick: price("0.1"),
        quantity_step: quantity("0.00000001"),
        max_levels_per_side: 10,
        max_buffered_deltas: 16,
        max_buffered_level_updates: 64,
        sequence_policy: SequencePolicy::ExactNext,
        checksum_policy: ChecksumPolicy::Required,
        max_l3_orders: None,
        max_l3_levels_per_side: None,
    }
}

fn l3_book_config() -> BookConfig {
    BookConfig {
        max_l3_orders: Some(5_000),
        max_l3_levels_per_side: Some(10),
        ..book_config()
    }
}

fn session() -> BookSession {
    BookSession {
        connection_epoch: 1,
        subscription_epoch: 1,
        instrument_generation: 1,
    }
}

fn price(value: &str) -> Price {
    Price::new(FixedDecimal::parse(value).unwrap()).unwrap()
}

fn quantity(value: &str) -> Quantity {
    Quantity::new(FixedDecimal::parse(value).unwrap()).unwrap()
}

fn native_level(price_text: &str, quantity_text: &str) -> NativeBookLevel {
    NativeBookLevel {
        price: price(price_text),
        quantity: quantity(quantity_text),
        price_text: price_text.to_owned(),
        quantity_text: quantity_text.to_owned(),
    }
}

fn source() -> SourceId {
    SourceId::new(SourceKind::Exchange, "kraken", 1).unwrap()
}

async fn durable_reference(payload: &[u8]) -> connector_core::DurableRawReference {
    let directory = tempfile::tempdir().unwrap();
    let mut segment_id = [0_u8; 16];
    segment_id.copy_from_slice(&blake3::hash(payload).as_bytes()[..16]);
    let metadata = SegmentMetadata::new(
        segment_id,
        1,
        "schema",
        "installation",
        "build",
        vec![StreamDescriptor::new(7, wal_stream_source_identity(&source()), "fixture").unwrap()],
    )
    .unwrap();
    let mut writer =
        SegmentedWalWriter::create(directory.path(), metadata, RotationPolicy::default(), 1)
            .unwrap();
    let authority = writer.append_authority();
    let receive_time = UnixNanos::new(1_760_325_052_800_000_000);
    let (proof, compression) = writer
        .append(
            RecordMetadata {
                flags: 0,
                stream_id: 7,
                connection_epoch: 3,
                record_sequence: 1,
                receive_wall_time_ns: receive_time.value(),
                receive_monotonic_time_ns: 9,
            },
            payload,
            9,
            receive_time.value(),
        )
        .unwrap()
        .into_parts();
    assert!(compression.is_none());
    let channel = DurableRawCaptureChannel::new(
        authority,
        source(),
        NonZeroU32::new(7).unwrap(),
        1,
        payload.len() + 1,
    )
    .unwrap();
    let (client, mut worker) = channel.split();
    let pending = client
        .try_submit(
            RawCapture::try_new(
                source(),
                NonZeroU32::new(7).unwrap(),
                NonZeroU64::new(3).unwrap(),
                NonZeroU64::new(1).unwrap(),
                receive_time,
                9,
                payload.to_vec().into_boxed_slice(),
            )
            .unwrap(),
        )
        .unwrap();
    worker.recv().await.unwrap().acknowledge(proof).unwrap();
    pending.wait().await.unwrap()
}
